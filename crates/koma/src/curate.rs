//! The organising pages (docs/design: select, move, details, trash, jobs):
//! what curators see and send. Every change is checked here (who may make
//! it, where it may go), queued in accounts.db for the worker
//! ([`crate::organise`]), and waited for a moment, so a quick one shows at
//! once; a long one carries on in the background, on `/jobs`.

use std::collections::HashMap;
use std::fmt::Write;
use std::time::Duration;

use axum::Extension;
use axum::extract::{Path as UrlPath, RawQuery, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE};
use axum::response::{Html, IntoResponse, Response};
use serde_json::{Value, json};

use crate::accounts::{Op, Role};
use crate::auth::{Viewer, field, form, see_other};
use crate::catalog::Catalog;
use crate::html::{Here, escape, human_size, shell};
use crate::journal::OpState;
use crate::libraries::{Library, library_of, owner_of};
use crate::organise;
use crate::serve::{AppState, Failure};

/// Whether `me` may organise what is in `library`: curators and the admin
/// the shared libraries, each account its own folder.
pub fn may_organise(me: &Viewer, library: &str) -> bool {
    match owner_of(library) {
        Some(owner) => owner == me.name,
        None => me.role >= Role::Curator,
    }
}

pub(crate) fn refuse(me: &Viewer) -> Failure {
    Failure::Forbidden(format!(
        "Only curators and the admin can organise the shared libraries. You are signed in as {}, a {}.",
        me.name,
        me.role.as_str()
    ))
}

/// All values of `key` in a form.
pub(crate) fn all(f: &[(String, String)], key: &str) -> Vec<String> {
    f.iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .collect()
}

pub(crate) fn all_ids(f: &[(String, String)], key: &str) -> Vec<i64> {
    all(f, key).iter().filter_map(|v| v.parse().ok()).collect()
}

/// The values of `key` in a query string.
pub(crate) fn query_all(q: Option<&str>, key: &str) -> Vec<String> {
    q.unwrap_or_default()
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (k == key)
                .then(|| crate::serve::percent_decode(v))
                .flatten()
        })
        .collect()
}

pub(crate) fn query_ids(q: Option<&str>, key: &str) -> Vec<i64> {
    query_all(q, key)
        .iter()
        .filter_map(|v| v.parse().ok())
        .collect()
}

/// Queues a change and waits up to a few seconds for the worker to finish
/// it. Returns its id and, if it finished, its state.
pub(crate) async fn queue(
    state: &AppState,
    me: &Viewer,
    kind: &str,
    payload: Value,
    title: String,
) -> Result<(i64, Option<OpState>), Failure> {
    let (id, name, kind) = (me.id, me.name.clone(), kind.to_owned());
    let payload = payload.to_string();
    let t = title.clone();
    let op = state
        .accounts(move |a| Ok(a.queue_op(id, &name, &kind, &payload, &t)?))
        .await?;
    crate::log::info(
        "serve: change queued",
        &[("change", &title), ("by", &me.name)],
    );
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let s = match state.query(move |c| Ok(c.op_state(op)?)).await {
            Ok(s) => s,
            Err(Failure::NotScanned) => None,
            Err(e) => return Err(e),
        };
        if let Some(s) = s
            && matches!(s.state.as_str(), "done" | "failed" | "cancelled")
        {
            return Ok((op, Some(s)));
        }
    }
    Ok((op, None))
}

/// Where to go after a change: `back` with `?done=<op>`.
pub(crate) fn back_to(back: &str, op: i64) -> Response {
    let back = crate::auth::safe_next(Some(back));
    let sep = if back.contains('?') { '&' } else { '?' };
    see_other(&format!("{back}{sep}done={op}"))
}

/// The note shown after a change: done (with undo), failed, or still going.
pub async fn toast(state: &AppState, me: &Viewer, op: i64) -> Option<String> {
    let me_id = me.id;
    let o = state
        .accounts(move |a| Ok(a.op(op)?))
        .await
        .ok()
        .flatten()?;
    if o.account != Some(me_id) {
        return None;
    }
    let s = state
        .query(move |c| Ok(c.op_state(op)?))
        .await
        .ok()
        .flatten();
    let title = escape(&o.title);
    Some(match s.as_ref().map(|s| s.state.as_str()) {
        Some("done") if organise::undoable(&o.kind) => format!(
            "<form class=toast role=status method=post action=/jobs/{op}/undo>Done: {title}.\
             <button class=btn type=submit>undo</button></form>"
        ),
        Some("done") => format!("<p class=toast role=status>Done: {title}.</p>"),
        Some("failed") => format!(
            "<p class=\"note err\" role=alert>{title}: {}</p>",
            escape(&s.map(|s| s.message).unwrap_or_default())
        ),
        _ => format!(
            "<p class=note role=status>{title}: going on in the background. <a href=/jobs>see jobs</a></p>"
        ),
    })
}

/// What the account has read of `books`: per book, the page or chapter
/// reached and whether it is finished.
pub async fn progress(
    state: &AppState,
    me: &Viewer,
    books: &[(i64, i64, bool)],
) -> Result<HashMap<i64, (i64, bool)>, Failure> {
    let keys: Vec<String> = books
        .iter()
        .map(|&(id, _, epub)| format!("{}:{id}", if epub { "epos" } else { "pos" }))
        .collect();
    let id = me.id;
    let kept: HashMap<String, String> = state
        .accounts(move |a| Ok(a.kept(id, &keys)?))
        .await?
        .into_iter()
        .collect();
    let mut out = HashMap::new();
    for &(book, total, epub) in books {
        let key = format!("{}:{book}", if epub { "epos" } else { "pos" });
        if let Some(v) = kept.get(&key) {
            let at: i64 = v
                .split(':')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            if at > 0 {
                // An EPUB is read once its last chapter is opened.
                out.insert(book, (at, at >= total));
            }
        }
    }
    Ok(out)
}

/// Every library, and those the viewer sees.
pub(crate) fn libraries(c: &Catalog, me: &Viewer) -> Result<Vec<Library>, Failure> {
    Ok(Library::all(c.library_keys()?, &c.library_settings()?)
        .into_iter()
        .filter(|l| l.visible_to(&me.name))
        .collect())
}

pub(crate) fn here<'a>(me: &'a crate::pages::Me<'a>, go: &'static str) -> Here<'a> {
    Here {
        library: None,
        collections: &[],
        collection: None,
        go,
        query: "",
        scroll: false,
        me,
    }
}

// Select mode's actions.

/// `POST /organise`: `action` (read, unread, move, details, trash,
/// trash-series), books `b`, series `s`.
pub async fn organise(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let action = field(&f, "action").to_owned();
    let books = all_ids(&f, "b");
    let series = field(&f, "s").parse::<i64>().unwrap_or(0);
    let back = format!("/s/{series}");
    let viewer = me.clone();
    let b2 = books.clone();
    // The books, as the viewer may see them: (id, pages, epub, series path, name).
    let (found, series_found) = state
        .query(move |c| {
            let mut found = Vec::new();
            for id in b2 {
                let Some(book) = c.book(id)? else { continue };
                let Some(s) = c.series_by_id(book.series_id)? else {
                    continue;
                };
                if viewer.sees(&s.path) {
                    found.push((
                        book.id,
                        book.page_count,
                        crate::scan::is_epub(&book.path),
                        s.path,
                        book.name,
                    ));
                }
            }
            let s = c.series_by_id(series)?.filter(|s| viewer.sees(&s.path));
            Ok((found, s))
        })
        .await?;
    let qs = |key: &str| -> String {
        found
            .iter()
            .map(|b| format!("{key}={}", b.0))
            .collect::<Vec<_>>()
            .join("&")
    };
    match action.as_str() {
        "read" | "unread" => {
            let (id, read) = (me.id, action == "read");
            let list: Vec<(i64, i64, bool)> = found.iter().map(|b| (b.0, b.1, b.2)).collect();
            state
                .accounts(move |a| {
                    for (book, total, epub) in list {
                        let key = format!("{}:{book}", if epub { "epos" } else { "pos" });
                        if read {
                            let v = if epub {
                                format!("{total}:0")
                            } else {
                                total.to_string()
                            };
                            a.keep(id, &[(key, v)], false)?;
                        } else {
                            a.unkeep(id, &[key])?;
                        }
                    }
                    Ok(())
                })
                .await?;
            Ok(see_other(&format!("{back}?select")))
        }
        "move" | "details" if found.is_empty() => Ok(see_other(&format!("{back}?select"))),
        "move" => Ok(see_other(&format!("/move?{}", qs("b")))),
        "details" => Ok(see_other(&format!("/details?{}", qs("b")))),
        "trash" => {
            if found.iter().any(|b| !may_organise(&me, library_of(&b.3))) {
                return Err(refuse(&me));
            }
            if found.is_empty() {
                return Ok(see_other(&format!("{back}?select")));
            }
            let names: Vec<&str> = found.iter().map(|b| b.4.as_str()).collect();
            let title = format!("Trash {}", list_names(&names));
            let ids: Vec<i64> = found.iter().map(|b| b.0).collect();
            let (op, _) = queue(&state, &me, "trash", json!({ "books": ids }), title).await?;
            Ok(back_to(&back, op))
        }
        "trash-series" => {
            let s = series_found.ok_or(Failure::NotFound)?;
            if !may_organise(&me, library_of(&s.path)) {
                return Err(refuse(&me));
            }
            let title = format!("Trash the series {}", s.name);
            let (op, _) = queue(&state, &me, "trash", json!({ "series": [s.id] }), title).await?;
            Ok(back_to("/trash", op))
        }
        _ => Err(Failure::NotFound),
    }
}

/// `A, B and 3 more`.
pub(crate) fn list_names(names: &[&str]) -> String {
    match names.len() {
        0 => "nothing".into(),
        1..=3 => names.join(", "),
        n => format!("{}, {} and {} more", names[0], names[1], n - 2),
    }
}

// Moving.

/// What a move asks, from the query (`GET /move`) or the form (`POST`):
/// books `b`, or a series `s`; to a series `to`, or a library `l`,
/// collection `c` and name `name`; or (a series) merged `into` another.
struct MoveAsk {
    books: Vec<i64>,
    series: Option<i64>,
    to: Option<i64>,
    into: Option<i64>,
    library: Option<String>,
    collection: String,
    name: String,
    skip: Vec<i64>,
}

impl MoveAsk {
    fn from(get: &dyn Fn(&str) -> Vec<String>) -> Self {
        let one = |k: &str| {
            get(k)
                .into_iter()
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned()
        };
        let id = |k: &str| one(k).parse::<i64>().ok();
        let ids = |k: &str| get(k).iter().filter_map(|v| v.parse().ok()).collect();
        Self {
            books: ids("b"),
            series: id("s"),
            to: id("to"),
            into: id("into"),
            library: get("l").into_iter().next(),
            collection: one("c"),
            name: one("name"),
            skip: ids("skip"),
        }
    }

    /// The change's kind and payload, if a destination was given.
    fn change(&self) -> Option<(&'static str, Value)> {
        if let Some(s) = self.series {
            if let Some(into) = self.into {
                return Some(("merge", json!({ "from": s, "into": into })));
            }
            let library = self.library.as_ref()?;
            return Some((
                "series",
                json!({ "series": s, "library": library, "collection": self.collection, "name": self.name }),
            ));
        }
        let to = match (self.to, &self.library) {
            (Some(id), _) => json!({ "series": id }),
            (None, Some(l)) if !self.name.is_empty() => {
                json!({ "library": l, "collection": self.collection, "name": self.name })
            }
            _ => return None,
        };
        Some((
            "move",
            json!({ "books": self.books, "to": to, "skip": self.skip }),
        ))
    }

    fn query(&self) -> String {
        let mut q: Vec<String> = self.books.iter().map(|b| format!("b={b}")).collect();
        if let Some(s) = self.series {
            q.push(format!("s={s}"));
        }
        q.join("&")
    }
}

/// What the move page needs from the catalogue.
struct MoveData {
    libraries: Vec<Library>,
    /// Books moved: (id, name, series id, series path).
    books: Vec<(i64, String, i64, String)>,
    series: Option<crate::catalog::Series>,
    /// Series the viewer may move to: (id, name, library name).
    targets: Vec<(i64, String, String)>,
    plan: Option<Result<organise::Plan, String>>,
}

async fn move_data(state: &AppState, me: &Viewer, ask: &MoveAsk) -> Result<MoveData, Failure> {
    let viewer = me.clone();
    let (books, series, to, into, change) = (
        ask.books.clone(),
        ask.series,
        ask.to,
        ask.into,
        ask.change(),
    );
    let library_root = state.library_root();
    state
        .query(move |c| {
            let libraries = libraries(c, &viewer)?;
            let mut found = Vec::new();
            for id in books {
                let book = c.book(id)?.ok_or(Failure::NotFound)?;
                let s = c.series_by_id(book.series_id)?.ok_or(Failure::NotFound)?;
                if !viewer.sees(&s.path) {
                    return Err(Failure::NotFound);
                }
                if !may_organise(&viewer, library_of(&s.path)) {
                    return Err(refuse(&viewer));
                }
                found.push((book.id, book.name, s.id, s.path));
            }
            let series = match series {
                Some(id) => {
                    let s = c
                        .series_by_id(id)?
                        .filter(|s| viewer.sees(&s.path))
                        .ok_or(Failure::NotFound)?;
                    if !may_organise(&viewer, library_of(&s.path)) {
                        return Err(refuse(&viewer));
                    }
                    Some(s)
                }
                None => None,
            };
            if found.is_empty() && series.is_none() {
                return Err(Failure::NotFound);
            }
            let names: HashMap<String, String> = libraries
                .iter()
                .map(|l| (l.folder.clone(), l.name.clone()))
                .collect();
            let mut targets: Vec<(i64, String, String)> = c
                .series()?
                .into_iter()
                .filter(|s| !s.path.starts_with('\0'))
                .filter(|s| {
                    names.contains_key(library_of(&s.path))
                        && may_organise(&viewer, library_of(&s.path))
                })
                .filter(|s| c.first_book(s.id).ok().flatten().is_some())
                .map(|s| {
                    let l = names.get(library_of(&s.path)).cloned().unwrap_or_default();
                    (s.id, s.name, l)
                })
                .collect();
            targets.sort_by(|a, b| crate::natsort::compare_paths(&a.1, &b.1));
            // The destination must be somewhere the viewer may organise.
            for id in to.into_iter().chain(into) {
                let t = c.series_by_id(id)?.ok_or(Failure::NotFound)?;
                if !viewer.sees(&t.path) || !may_organise(&viewer, library_of(&t.path)) {
                    return Err(refuse(&viewer));
                }
            }
            if let Some((_, p)) = &change
                && let Some(l) = p
                    .get("library")
                    .or_else(|| p.get("to").and_then(|t| t.get("library")))
                    .and_then(Value::as_str)
            {
                if !names.contains_key(l) && !(owner_of(l) == Some(&viewer.name)) {
                    return Err(Failure::NotFound);
                }
                if !may_organise(&viewer, l) {
                    return Err(refuse(&viewer));
                }
            }
            let plan = change.map(|(kind, p)| organise::plan(c, &library_root, kind, &p));
            Ok(MoveData {
                libraries,
                books: found,
                series,
                targets,
                plan,
            })
        })
        .await
}

/// `GET /move`: where to, then the plan as a dry run.
pub async fn move_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    RawQuery(q): RawQuery,
) -> Result<Response, Failure> {
    let get = |k: &str| query_all(q.as_deref(), k);
    let ask = MoveAsk::from(&get);
    let d = move_data(&state, &me, &ask).await?;
    Ok(Html(move_html(&me, &ask, &d)).into_response())
}

/// `POST /move`: carries out the plan.
pub async fn move_apply(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let get = |k: &str| all(&f, k);
    let ask = MoveAsk::from(&get);
    let d = move_data(&state, &me, &ask).await?;
    let Some((kind, payload)) = ask.change() else {
        return Ok(see_other(&format!("/move?{}", ask.query())));
    };
    if let Some(Err(e)) = &d.plan {
        return Ok((
            StatusCode::BAD_REQUEST,
            Html(move_html(&me, &ask, &d).replacen(
                "<main>",
                &format!("<main><p class=\"note err\">{}</p>", escape(e)),
                1,
            )),
        )
            .into_response());
    }
    let what = match (&d.series, d.books.len()) {
        (Some(s), _) => s.name.clone(),
        (None, 1) => d.books[0].1.clone(),
        (None, n) => format!(
            "{n} books of {}",
            d.books[0].3.rsplit('/').next().unwrap_or_default()
        ),
    };
    let target_name = |id: i64| {
        d.targets
            .iter()
            .find(|t| t.0 == id)
            .map_or(String::new(), |t| t.1.clone())
    };
    let title = match kind {
        "merge" => format!("Merge {what} into {}", target_name(ask.into.unwrap_or(0))),
        "series" => {
            let from = d
                .series
                .as_ref()
                .map(|s| s.path.clone())
                .unwrap_or_default();
            format!(
                "Move {from} → {}",
                [
                    ask.library.as_deref().unwrap_or(""),
                    &ask.collection,
                    &ask.name
                ]
                .iter()
                .filter(|p| !p.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join("/")
            )
        }
        _ => match ask.to {
            Some(id) => format!("Move {what} to {}", target_name(id)),
            None => format!("Move {what} to a new series, {}", ask.name),
        },
    };
    let (op, done) = queue(&state, &me, kind, payload, title).await?;
    // Where the books went: their series' page.
    let back = match (&done, kind) {
        (Some(s), _) if s.state != "done" => "/jobs".to_owned(),
        (None, _) => "/jobs".to_owned(),
        (_, "series") => format!("/s/{}", ask.series.unwrap_or(0)),
        (_, "merge") => format!("/s/{}", ask.into.unwrap_or(0)),
        _ => {
            let first = ask.books.first().copied().unwrap_or(0);
            let s = state
                .query(move |c| Ok(c.book(first)?.map(|b| b.series_id)))
                .await?
                .unwrap_or(0);
            format!("/s/{s}")
        }
    };
    Ok(back_to(&back, op))
}

fn move_html(me: &Viewer, ask: &MoveAsk, d: &MoveData) -> String {
    let mut m = String::new();
    let what = match (&d.series, d.books.len()) {
        (Some(s), _) => format!("the series {}", s.name),
        (None, 1) => d.books[0].1.clone(),
        (None, n) => format!("{n} books"),
    };
    let _ = write!(
        m,
        "<div class=toolbar><h1>move · <b>{}</b></h1></div><div class=pair>",
        escape(&what)
    );
    let hidden = |m: &mut String| {
        for b in &ask.books {
            let _ = write!(m, "<input type=hidden name=b value={b}>");
        }
        if let Some(s) = ask.series {
            let _ = write!(m, "<input type=hidden name=s value={s}>");
        }
    };
    let library_select = |m: &mut String, chosen: Option<&str>| {
        m.push_str("<label>library<select name=l>");
        for l in &d.libraries {
            let _ = write!(
                m,
                "<option value=\"{}\"{}>{}</option>",
                escape(&l.folder),
                if chosen == Some(l.folder.as_str()) {
                    " selected"
                } else {
                    ""
                },
                escape(&l.name)
            );
        }
        m.push_str("</select></label>");
    };
    let series_select = |m: &mut String, name: &str, label: &str, chosen: Option<i64>| {
        let _ = write!(m, "<label>{label}<select name={name}>");
        for (id, n, l) in &d.targets {
            if d.series.as_ref().is_some_and(|s| s.id == *id) {
                continue;
            }
            let _ = write!(
                m,
                "<option value={id}{}>{} · {}</option>",
                if chosen == Some(*id) { " selected" } else { "" },
                escape(n),
                escape(l)
            );
        }
        m.push_str("</select></label>");
    };
    if let Some(s) = &d.series {
        let lib = library_of(&s.path);
        let rest = s
            .path
            .strip_prefix(lib)
            .map(|r| r.trim_start_matches('/'))
            .unwrap_or(&s.path);
        let (collection, name) = rest.rsplit_once('/').unwrap_or(("", rest));
        m.push_str("<form class=\"box form\" method=get><h2>move or rename the folder</h2>");
        hidden(&mut m);
        library_select(&mut m, Some(ask.library.as_deref().unwrap_or(lib)));
        let _ = write!(
            m,
            "<label>collection<input name=c value=\"{}\" placeholder=\"none\"><small>A folder of series inside the library, or nothing.</small></label>\
             <label>name<input name=name value=\"{}\" required></label>\
             <button class=btn type=submit>show the plan</button></form>",
            escape(if ask.library.is_some() {
                &ask.collection
            } else {
                collection
            }),
            escape(if ask.name.is_empty() { name } else { &ask.name })
        );
        m.push_str("<form class=\"box form\" method=get><h2>or merge it into</h2>");
        hidden(&mut m);
        series_select(&mut m, "into", "series", ask.into);
        m.push_str("<button class=btn type=submit>show the plan</button><small>Its books join that series' folder; its own folder goes if nothing else is left in it.</small></form>");
    } else {
        m.push_str("<form class=\"box form\" method=get><h2>to a series</h2>");
        hidden(&mut m);
        series_select(&mut m, "to", "series", ask.to);
        m.push_str("<button class=btn type=submit>show the plan</button></form>");
        m.push_str("<form class=\"box form\" method=get><h2>or a new series</h2>");
        hidden(&mut m);
        library_select(&mut m, ask.library.as_deref());
        let _ = write!(
            m,
            "<label>collection<input name=c value=\"{}\" placeholder=\"none\"></label>\
             <label>name<input name=name value=\"{}\" required></label>\
             <button class=btn type=submit>show the plan</button></form>",
            escape(&ask.collection),
            escape(&ask.name)
        );
    }
    m.push_str("</div>");
    match &d.plan {
        None => m.push_str(
            "<p class=hint>Pick where they go: the plan, a dry run, shows every file's old and new place \
             before anything changes.</p>",
        ),
        Some(Err(e)) => {
            let _ = write!(m, "<p class=\"note err\">{}</p>", escape(e));
        }
        Some(Ok(plan)) => {
            let clashes = plan.steps.iter().filter(|s| s.clash).count();
            let _ = write!(
                m,
                "<h2 class=section-h>plan · dry run · {} to move{}</h2><form method=post action=/move>",
                plan.steps.len(),
                if clashes > 0 { format!(" · {clashes} name taken") } else { String::new() }
            );
            hidden(&mut m);
            if let Some(l) = &ask.library {
                let _ = write!(
                    m,
                    "<input type=hidden name=l value=\"{}\"><input type=hidden name=c value=\"{}\"><input type=hidden name=name value=\"{}\">",
                    escape(l),
                    escape(&ask.collection),
                    escape(&ask.name)
                );
            }
            for (k, v) in [("to", ask.to), ("into", ask.into)] {
                if let Some(v) = v {
                    let _ = write!(m, "<input type=hidden name={k} value={v}>");
                }
            }
            m.push_str("<table class=plan><tbody>");
            if plan.steps.is_empty() {
                m.push_str("<tr><td>Nothing to move: they are there already.</td></tr>");
            }
            for (i, s) in plan.steps.iter().enumerate() {
                let _ = write!(
                    m,
                    "<tr{}><td>{}</td><td><div class=from>{}</div><div class=to>{}</div>{}</td><td>",
                    if s.clash { " class=warn" } else { "" },
                    i + 1,
                    escape(&s.from),
                    escape(&s.to),
                    if s.clash {
                        "<div class=hint>That name is taken there: “(2)” is added.</div>"
                    } else {
                        ""
                    }
                );
                match (s.clash, s.book) {
                    (true, Some(b)) => {
                        let _ = write!(m, "<label class=hint><input type=checkbox name=skip value={b}> skip</label>");
                    }
                    _ => m.push_str(if s.folder { "folder" } else { "move" }),
                }
            }
            m.push_str(
                "</tbody></table><div class=actbar><button class=\"btn primary\" type=submit>apply</button>\
                 <a class=btn href=/>cancel</a><span class=hint>Each book keeps its id, reading positions, \
                 edits and date added. A short plan applies at once; a long one runs as a job you can pause. \
                 Undo puts everything back.</span></div></form>",
            );
        }
    }
    let me2 = me.me();
    shell("Move", &d.libraries, &here(&me2, "library"), &m)
}

// Details.

/// The edited fields, as the form names them.
const TEXT_FIELDS: [(&str, &str); 4] = [
    ("title", "title"),
    ("summary", "summary"),
    ("number", "number"),
    ("year", "year"),
];
const LIST_FIELDS: [(&str, &str); 4] = [
    ("authors", "authors"),
    ("publishers", "publishers"),
    ("genres", "genres"),
    ("tags", "tags"),
];

/// A book's details as the form shows them: field → text.
fn form_values(d: &crate::catalog::StoredDetails) -> HashMap<&'static str, String> {
    let mut v = HashMap::new();
    v.insert("title", d.title.clone());
    v.insert("summary", d.summary.clone());
    v.insert(
        "number",
        d.number.map(|n| n.to_string()).unwrap_or_default(),
    );
    v.insert("year", d.year.map(|n| n.to_string()).unwrap_or_default());
    v.insert("kind", d.kind.clone().unwrap_or_default());
    v.insert("direction", d.direction.clone().unwrap_or_default());
    v.insert(
        "cover",
        d.cover_page
            .map(|n| (n + 1).to_string())
            .unwrap_or_default(),
    );
    v.insert("authors", d.authors.join(", "));
    v.insert("publishers", d.publishers.join(", "));
    v.insert("genres", d.genres.join(", "));
    v.insert("tags", d.tags.join(", "));
    v
}

struct DetailsData {
    libraries: Vec<Library>,
    /// (id, name, page count, series id)
    books: Vec<(i64, String, i64, i64)>,
    /// Per field: the value all books share, or `None` (mixed).
    values: HashMap<&'static str, Option<String>>,
    /// The first book's first pages, for the cover picker: (number, url).
    pages: Vec<(i64, String)>,
}

async fn details_data(
    state: &AppState,
    me: &Viewer,
    books: Vec<i64>,
) -> Result<DetailsData, Failure> {
    let viewer = me.clone();
    state
        .query(move |c| {
            let libraries = libraries(c, &viewer)?;
            let mut found = Vec::new();
            let mut values: HashMap<&'static str, Option<String>> = HashMap::new();
            for (i, id) in books.iter().enumerate() {
                let book = c.book(*id)?.ok_or(Failure::NotFound)?;
                let s = c.series_by_id(book.series_id)?.ok_or(Failure::NotFound)?;
                if !viewer.sees(&s.path) {
                    return Err(Failure::NotFound);
                }
                if !may_organise(&viewer, library_of(&s.path)) {
                    return Err(refuse(&viewer));
                }
                let d = c.details(&book.path)?.unwrap_or_default();
                for (k, v) in form_values(&d) {
                    if i == 0 {
                        values.insert(k, Some(v));
                    } else if values.get(k).and_then(|x| x.as_ref()) != Some(&v) {
                        values.insert(k, None);
                    }
                }
                found.push((book.id, book.name, book.page_count, book.series_id));
            }
            if found.is_empty() {
                return Err(Failure::NotFound);
            }
            let mut pages = Vec::new();
            if found.len() == 1 {
                for p in c.pages(found[0].0)?.into_iter().take(12) {
                    let url = match c
                        .variants_of(&p.hash)?
                        .iter()
                        .find(|v| v.0 == crate::html::COVER_WIDTH && v.2 != "avif")
                    {
                        Some(v) => match crate::variants::Format::from_ext(&v.2) {
                            Some(f) => crate::variants::url(&p.hash, v.0, f),
                            None => crate::html::page_url(&p),
                        },
                        None => crate::html::page_url(&p),
                    };
                    pages.push((p.number, url));
                }
            }
            Ok(DetailsData {
                libraries,
                books: found,
                values,
                pages,
            })
        })
        .await
}

/// `GET /details?b=…`: one book's details, or several at once.
pub async fn details_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    RawQuery(q): RawQuery,
) -> Result<Response, Failure> {
    let d = details_data(&state, &me, query_ids(q.as_deref(), "b")).await?;
    Ok(Html(details_html(&me, &d)).into_response())
}

fn details_html(me: &Viewer, d: &DetailsData) -> String {
    let many = d.books.len() > 1;
    let mut m = String::new();
    let _ = write!(
        m,
        "<div class=toolbar><h1>details · <b>{}</b></h1>{}</div><form method=post action=/details>",
        escape(&if many {
            format!("{} books", d.books.len())
        } else {
            d.books[0].1.clone()
        }),
        if many {
            "<span class=hint>several books: a field shown as “mixed” keeps each book's own unless you type in it</span>"
        } else {
            ""
        }
    );
    for b in &d.books {
        let _ = write!(m, "<input type=hidden name=b value={}>", b.0);
    }
    let value = |k: &str| d.values.get(k).cloned().flatten();
    let input = |m: &mut String, k: &str, label: &str, extra: &str| {
        let v = value(k);
        let _ = write!(
            m,
            "<label>{label}<input name={k} value=\"{}\"{}{extra}><input type=hidden name=was_{k} value=\"{}\"></label>",
            escape(v.as_deref().unwrap_or_default()),
            if v.is_none() {
                " placeholder=mixed"
            } else {
                ""
            },
            escape(v.as_deref().unwrap_or("\u{1}mixed"))
        );
    };
    m.push_str("<div class=pair><section class=\"box form\"><h2>book</h2>");
    input(&mut m, "title", "title", "");
    m.push_str("<div class=row>");
    input(&mut m, "number", "number", " size=6");
    input(&mut m, "year", "year", " size=6 inputmode=numeric");
    for (k, label, options) in [
        (
            "kind",
            "kind",
            &[("", "—"), ("volume", "volume"), ("chapter", "chapter")][..],
        ),
        (
            "direction",
            "direction",
            &[
                ("", "from the library"),
                ("rtl", "right to left"),
                ("ltr", "left to right"),
            ][..],
        ),
    ] {
        let v = value(k);
        let _ = write!(m, "<label>{label}<select name={k}>");
        if v.is_none() {
            m.push_str("<option value=\"\u{1}mixed\" selected>mixed</option>");
        }
        for (val, text) in options {
            let _ = write!(
                m,
                "<option value=\"{val}\"{}>{text}</option>",
                if v.as_deref() == Some(*val) {
                    " selected"
                } else {
                    ""
                }
            );
        }
        let _ = write!(
            m,
            "</select><input type=hidden name=was_{k} value=\"{}\"></label>",
            escape(v.as_deref().unwrap_or("\u{1}mixed"))
        );
    }
    let summary = value("summary");
    let _ = write!(
        m,
        "</div><label>summary<textarea name=summary{}>{}</textarea><input type=hidden name=was_summary value=\"{}\"></label></section>",
        if summary.is_none() {
            " placeholder=mixed"
        } else {
            ""
        },
        escape(summary.as_deref().unwrap_or_default()),
        escape(summary.as_deref().unwrap_or("\u{1}mixed"))
    );
    m.push_str("<section class=\"box form\"><h2>people and tags</h2>");
    for (k, label) in LIST_FIELDS {
        input(&mut m, k, label, "");
    }
    m.push_str("<small>Separate names with commas.</small></section></div>");
    if !d.pages.is_empty() {
        let cover = value("cover")
            .and_then(|c| c.parse::<i64>().ok())
            .unwrap_or(1);
        m.push_str("<section class=box><h2>cover</h2><div class=covers>");
        for (n, url) in &d.pages {
            let _ = write!(
                m,
                "<label><input type=radio name=cover value={}{}><div class=cover><img src=\"{url}\" alt=\"page {}\" loading=lazy></div></label>",
                n + 1,
                if n + 1 == cover { " checked" } else { "" },
                n + 1
            );
        }
        let _ = write!(
            m,
            "</div><input type=hidden name=was_cover value={cover}></section>"
        );
    }
    let back = format!("/s/{}", d.books[0].3);
    let _ = write!(
        m,
        "<div class=actbar><button class=\"btn primary\" type=submit>save</button>\
         <label class=\"row hint\"><input type=checkbox name=write value=1> also write <code>metadata.json</code> next to {}</label>\
         <a class=btn href=\"{back}\">cancel</a></div></form>\
         <div class=status><p>Saved in the catalogue at once, and kept over what the files say at the next scan. \
         Nothing in /library changes unless you ask for <code>metadata.json</code>: it is written next to the book \
         (inside a photo folder), and the book's own bytes are never rewritten.</p></div>",
        if many { "each book" } else { "the book" }
    );
    let me2 = me.me();
    shell("Details", &d.libraries, &here(&me2, "library"), &m)
}

/// `POST /details`: the fields changed (compared with `was_<field>`).
pub async fn details_save(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let books = all_ids(&f, "b");
    let d = details_data(&state, &me, books.clone()).await?;
    let mut set = serde_json::Map::new();
    let changed = |k: &str| -> Option<String> {
        let now = field(&f, k);
        let was = field(&f, &format!("was_{k}"));
        (now != was && now != "\u{1}mixed" && !(now.is_empty() && was == "\u{1}mixed"))
            .then(|| now.trim().to_owned())
    };
    for (k, _) in TEXT_FIELDS {
        if let Some(v) = changed(k) {
            let value = match k {
                "number" => v
                    .replace(',', ".")
                    .parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
                    .map_or(Value::Null, |n| json!(n)),
                "year" => v
                    .parse::<i32>()
                    .ok()
                    .filter(|y| (0..=9999).contains(y))
                    .map_or(Value::Null, |y| json!(y)),
                _ => json!(
                    v.chars()
                        .take(if k == "summary" { 20_000 } else { 500 })
                        .collect::<String>()
                ),
            };
            set.insert(k.to_owned(), value);
        }
    }
    for k in ["kind", "direction"] {
        if let Some(v) = changed(k) {
            let ok = matches!(
                (k, v.as_str()),
                ("kind", "volume" | "chapter") | ("direction", "rtl" | "ltr")
            );
            set.insert(k.to_owned(), if ok { json!(v) } else { Value::Null });
        }
    }
    if let Some(v) = changed("cover") {
        set.insert(
            "cover".into(),
            v.parse::<i64>()
                .ok()
                .filter(|&n| n >= 1)
                .map_or(Value::Null, |n| json!(n - 1)),
        );
    }
    for (k, _) in LIST_FIELDS {
        if let Some(v) = changed(k) {
            let names: Vec<String> = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(200).collect())
                .take(100)
                .collect();
            set.insert(k.to_owned(), json!(names));
        }
    }
    let write = !field(&f, "write").is_empty();
    if set.is_empty() && !write {
        return Ok(see_other(&format!("/s/{}", d.books[0].3)));
    }
    let what = if d.books.len() == 1 {
        d.books[0].1.clone()
    } else {
        format!("{} books", d.books.len())
    };
    let title = if set.is_empty() {
        format!("Write metadata.json for {what}")
    } else {
        format!(
            "Details of {what}{}",
            if write {
                ", written to metadata.json"
            } else {
                ""
            }
        )
    };
    let (kind, payload) = if set.is_empty() {
        ("write", json!({ "books": books }))
    } else {
        (
            "details",
            json!({ "books": books, "set": set, "write": write }),
        )
    };
    let (op, _) = queue(&state, &me, kind, payload, title).await?;
    Ok(back_to(&format!("/s/{}", d.books[0].3), op))
}

// The trash.

/// Trash items the viewer may see and act on.
fn trash_for(c: &Catalog, me: &Viewer) -> Result<Vec<crate::journal::TrashItem>, Failure> {
    Ok(c.trash_items()?
        .into_iter()
        .filter(|t| me.sees(&t.from) && may_organise(me, library_of(&t.from)))
        .collect())
}

/// `GET /trash`.
pub async fn trash_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    RawQuery(q): RawQuery,
) -> Result<Response, Failure> {
    let viewer = me.clone();
    let (libraries, items) = match state
        .query(move |c| Ok((libraries(c, &viewer)?, trash_for(c, &viewer)?)))
        .await
    {
        Err(Failure::NotScanned) => (Vec::new(), Vec::new()),
        other => other?,
    };
    let days = state.accounts(|a| Ok(a.trash_days()?)).await?;
    let toast = match crate::serve::query_value(q.as_deref(), "done").and_then(|d| d.parse().ok()) {
        Some(op) => toast(&state, &me, op).await,
        None => None,
    };
    let now = crate::accounts::now();
    let total: i64 = items.iter().map(|t| t.size).sum();
    let mut m = toast.unwrap_or_default();
    let _ = write!(
        m,
        "<div class=toolbar><h1>trash · <b>{} · {}</b></h1>",
        crate::html::plural(items.len() as i64, "item", "items"),
        human_size(total as u64)
    );
    if me.is_admin() {
        m.push_str("<form class=row method=post action=/trash><label class=hint>keep for <select name=days>");
        for (d, label) in [
            (7, "7 days"),
            (14, "14 days"),
            (30, "30 days"),
            (90, "90 days"),
            (0, "until emptied"),
        ] {
            let _ = write!(
                m,
                "<option value={d}{}>{label}</option>",
                if d == days { " selected" } else { "" }
            );
        }
        m.push_str("</select></label><button class=btn type=submit name=action value=keep>save</button></form>");
    } else {
        let _ = write!(
            m,
            "<span class=hint>kept {}</span>",
            if days > 0 {
                format!("{days} days")
            } else {
                "until emptied".into()
            }
        );
    }
    m.push_str("</div>");
    if items.is_empty() {
        m.push_str("<div class=empty><strong>The trash is empty</strong><p>A delete moves books here first; they stay for the keep time, then go for good.</p></div>");
    } else {
        m.push_str(
            "<form method=post action=/trash><div class=scroll><table class=tbl><thead><tr><th scope=col><span class=hint>pick</span>\
             <th scope=col>what<th scope=col class=opt>where it was<th scope=col class=r>size<th scope=col class=opt>deleted\
             <th scope=col>days left</thead><tbody>",
        );
        for t in &items {
            let left = if days > 0 {
                let l = (t.at + days * 86_400 - now).div_euclid(86_400).max(0);
                format!(
                    "<td class=\"left{}\">{l}",
                    if l <= 3 { " soon" } else { "" }
                )
            } else {
                "<td>—".into()
            };
            let folder = t.from.rsplit_once('/').map_or("/library", |(d, _)| d);
            let _ = write!(
                m,
                "<tr><td><input type=checkbox name=t value={} aria-label=\"{}\"><td>{}{}<td class=opt>{}<td class=r>{}<td class=opt>{} · {}{left}",
                t.id,
                escape(&t.name),
                escape(&t.name),
                if t.kind == "series" {
                    " <span class=hint>series</span>"
                } else {
                    ""
                },
                escape(folder),
                human_size(t.size as u64),
                escape(&t.by),
                crate::pages::ago(t.at, now)
            );
        }
        m.push_str(
            "</tbody></table></div><div class=actbar><button class=\"btn primary\" type=submit name=action value=restore>restore</button>\
             <button class=\"btn danger\" type=submit name=action value=erase>delete for good</button>\
             <button class=\"btn danger\" type=submit name=action value=empty>empty the trash</button></div></form>",
        );
    }
    m.push_str(
        "<h2 class=section-h>how it works</h2><div class=status><p>A delete moves the files into <code>/library/.koma-trash</code>, \
         on the same drive: instant, nothing copied. The books leave the library at once and keep their ids, reading \
         positions and edits.</p><p>Restore puts them back where they were, making any folder that has gone since, and \
         adds “(2)” to a name that is taken now.</p><p>After the keep time the worker erases them. “Delete for good” \
         and “empty the trash” cannot be undone.</p></div>",
    );
    let me2 = me.me();
    Ok(Html(shell("Trash", &libraries, &here(&me2, "trash"), &m)).into_response())
}

/// `POST /trash`: action restore, erase or empty (items `t`), or keep
/// (`days`, the admin's).
pub async fn trash_action(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let action = field(&f, "action").to_owned();
    if action == "keep" {
        crate::auth::admin_only(&me, "set how long the trash keeps things")?;
        let days: i64 = field(&f, "days").parse().unwrap_or(30);
        if ![0, 7, 14, 30, 90].contains(&days) {
            return Err(Failure::NotFound);
        }
        state.accounts(move |a| Ok(a.set_trash_days(days)?)).await?;
        return Ok(see_other("/trash"));
    }
    let picked = all_ids(&f, "t");
    let viewer = me.clone();
    let items = state.query(move |c| trash_for(c, &viewer)).await?;
    // Anything picked the viewer may not touch: refused.
    if picked.iter().any(|p| !items.iter().any(|t| t.id == *p))
        || (action == "empty" && items.is_empty() && me.role < Role::Curator)
    {
        return Err(refuse(&me));
    }
    let chosen: Vec<&crate::journal::TrashItem> = match action.as_str() {
        "empty" => items.iter().collect(),
        _ => items.iter().filter(|t| picked.contains(&t.id)).collect(),
    };
    if chosen.is_empty() {
        return Ok(see_other("/trash"));
    }
    let ids: Vec<i64> = chosen.iter().map(|t| t.id).collect();
    let names: Vec<&str> = chosen.iter().map(|t| t.name.as_str()).collect();
    let (kind, title) = match action.as_str() {
        "restore" => ("restore", format!("Restore {}", list_names(&names))),
        "erase" | "empty" => ("erase", format!("Delete {} for good", list_names(&names))),
        _ => return Err(Failure::NotFound),
    };
    let (op, _) = queue(&state, &me, kind, json!({ "items": ids }), title).await?;
    Ok(back_to("/trash", op))
}

// Jobs.

/// The changes the viewer may see: the admin every one, others their own.
async fn visible_ops(state: &AppState, me: &Viewer) -> Result<Vec<(Op, Option<OpState>)>, Failure> {
    let (id, admin) = (me.id, me.is_admin());
    let ops: Vec<Op> = state
        .accounts(move |a| Ok(a.recent_ops(40)?))
        .await?
        .into_iter()
        .filter(|o| admin || o.account == Some(id))
        .collect();
    let ids: Vec<i64> = ops.iter().map(|o| o.id).collect();
    let states = match state
        .query(move |c| {
            let mut out = HashMap::new();
            for id in ids {
                if let Some(s) = c.op_state(id)? {
                    out.insert(id, s);
                }
            }
            Ok(out)
        })
        .await
    {
        Err(Failure::NotScanned) => HashMap::new(),
        other => other?,
    };
    let mut out: Vec<(Op, Option<OpState>)> = ops
        .into_iter()
        .map(|o| {
            let s = states.get(&o.id).cloned();
            (o, s)
        })
        .collect();
    // The worker's own changes (the cosplay tidy), for those who organise.
    if me.role >= Role::Curator {
        let worker = match state.query(|c| Ok(c.worker_ops(10)?)).await {
            Err(Failure::NotScanned) => Vec::new(),
            other => other?,
        };
        for (s, kind, title) in worker {
            out.push((
                Op {
                    id: s.id,
                    account: None,
                    by: "the worker".into(),
                    kind,
                    payload: String::new(),
                    title,
                    control: String::new(),
                    created: s.started,
                },
                Some(s),
            ));
        }
        out.sort_by_key(|(o, _)| std::cmp::Reverse(o.created));
    }
    Ok(out)
}

/// `GET /jobs`.
pub async fn jobs_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
) -> Result<Response, Failure> {
    let ops = visible_ops(&state, &me).await?;
    let viewer = me.clone();
    let mine: Vec<i64> = ops.iter().map(|(o, _)| o.id).collect();
    let (libraries, journal) = match state
        .query(move |c| {
            let journal: Vec<crate::journal::Entry> = c
                .journal_recent(200)?
                .into_iter()
                .filter(|e| mine.contains(&e.op))
                .take(30)
                .collect();
            Ok((libraries(c, &viewer)?, journal))
        })
        .await
    {
        Err(Failure::NotScanned) => (Vec::new(), Vec::new()),
        other => other?,
    };
    let now = crate::accounts::now();
    let active: Vec<&(Op, Option<OpState>)> = ops
        .iter()
        .filter(|(_, s)| {
            s.as_ref()
                .is_none_or(|s| matches!(s.state.as_str(), "running" | "paused"))
        })
        .collect();
    let finished: Vec<&(Op, Option<OpState>)> = ops
        .iter()
        .filter(|(_, s)| {
            s.as_ref()
                .is_some_and(|s| !matches!(s.state.as_str(), "running" | "paused"))
        })
        .collect();
    let mut m = String::new();
    let _ = write!(
        m,
        "<div class=toolbar><h1>jobs · <b>{}</b></h1></div><div data-jobs>",
        if active.is_empty() {
            "nothing running".to_owned()
        } else {
            format!("{} going", active.len())
        }
    );
    for (o, s) in &active {
        job(&mut m, o, s.as_ref(), now);
    }
    m.push_str("</div>");
    if !finished.is_empty() {
        m.push_str("<h2 class=section-h>done · undo works while the files are as the change left them</h2>");
        for (o, s) in &finished {
            job(&mut m, o, s.as_ref(), now);
        }
    }
    if !journal.is_empty() {
        m.push_str(
            "<h2 class=section-h>journal · append-only, one line per file</h2><div class=scroll><table class=tbl>\
             <thead><tr><th scope=col>when<th scope=col>change<th scope=col>did<th scope=col class=opt>from → to</thead><tbody>",
        );
        for e in &journal {
            let _ = write!(
                m,
                "<tr><td>{}<td>#{}<td>{}<td class=opt>{}",
                crate::pages::ago(e.at, now),
                e.op,
                escape(&e.action),
                if e.from.is_empty() && e.to.is_empty() {
                    e.book.map_or_else(String::new, |b| format!("book {b}"))
                } else {
                    format!("{} → {}", escape(&e.from), escape(&e.to))
                }
            );
        }
        m.push_str("</tbody></table></div>");
    }
    m.push_str(
        "<p class=hint>The page follows the jobs live (one event stream); without JavaScript it shows where \
         they were when it was loaded. Jobs carry on after a restart.</p>",
    );
    let me2 = me.me();
    let page = shell("Jobs", &libraries, &here(&me2, "jobs"), &m);
    Ok(Html(page.replacen(
        "</head>",
        &format!(
            "<script defer src=\"{}\"></script></head>",
            curator_js_url()
        ),
        1,
    ))
    .into_response())
}

fn job(m: &mut String, o: &Op, s: Option<&OpState>, now: i64) {
    let state = s.map_or("waiting", |s| s.state.as_str());
    let (done, total) = s.map_or((0, 0), |s| (s.done, s.total));
    let pct = if total > 0 { done * 100 / total } else { 0 };
    let _ = write!(
        m,
        "<section class=\"job{}\" data-op={} data-state={state}><div><b>{}</b><br><small>by {} · {} · <span data-text>{}</span></small></div><form class=row method=post>",
        if matches!(state, "running" | "paused" | "waiting") {
            ""
        } else {
            " done"
        },
        o.id,
        escape(&o.title),
        escape(&o.by),
        crate::pages::ago(o.created, now),
        escape(&match state {
            "running" => format!("{done} of {total}"),
            "paused" => format!("paused at {done} of {total}"),
            "waiting" => "waiting".to_owned(),
            "done" => "done".to_owned(),
            "cancelled" => s.map(|s| s.message.clone()).unwrap_or_default(),
            _ => format!(
                "failed: {}",
                s.map(|s| s.message.as_str()).unwrap_or_default()
            ),
        })
    );
    let button = |m: &mut String, action: &str, class: &str| {
        let _ = write!(
            m,
            "<button class=\"btn{class}\" type=submit formaction=/jobs/{}/{action}>{action}</button>",
            o.id
        );
    };
    match state {
        "running" | "waiting" => {
            button(m, "pause", "");
            button(m, "cancel", " danger");
        }
        "paused" => {
            button(m, "resume", "");
            button(m, "cancel", " danger");
        }
        "done" | "cancelled" if organise::undoable(&o.kind) => button(m, "undo", ""),
        _ => {}
    }
    m.push_str("</form>");
    if matches!(state, "running" | "paused") {
        let _ = write!(
            m,
            "<div class=meter role=progressbar aria-valuenow={pct} aria-valuemin=0 aria-valuemax=100><i style=\"--p:{pct}%\"></i></div>"
        );
    }
    m.push_str("</section>");
}

/// `POST /jobs/<id>/<action>`: pause, resume, cancel or undo.
pub async fn job_action(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    UrlPath((id, action)): UrlPath<(i64, String)>,
) -> Result<Response, Failure> {
    let op = if id < 0 {
        // One of the worker's own changes: undo only, by those who organise.
        if me.role < Role::Curator || action != "undo" {
            return Err(Failure::NotFound);
        }
        let (kind, title) = state
            .query(move |c| Ok(c.worker_op_kind(id)?))
            .await?
            .ok_or(Failure::NotFound)?;
        Op {
            id,
            account: None,
            by: "the worker".into(),
            kind,
            payload: String::new(),
            title,
            control: String::new(),
            created: 0,
        }
    } else {
        state
            .accounts(move |a| Ok(a.op(id)?))
            .await?
            .ok_or(Failure::NotFound)?
    };
    if id > 0 && op.account != Some(me.id) && !me.is_admin() {
        return Err(Failure::NotFound);
    }
    match action.as_str() {
        "pause" | "resume" | "cancel" => {
            let control = match action.as_str() {
                "pause" => "pause",
                "cancel" => "cancel",
                _ => "",
            }
            .to_owned();
            state
                .accounts(move |a| Ok(a.set_control(id, &control)?))
                .await?;
            Ok(see_other("/jobs"))
        }
        "undo" => {
            if !organise::undoable(&op.kind) {
                return Err(Failure::NotFound);
            }
            if me.role < Role::Curator && !op_in_own_folder(&op, &me) {
                return Err(refuse(&me));
            }
            let (undo, _) = queue(
                &state,
                &me,
                "undo",
                json!({ "op": id }),
                format!("Undo: {}", op.title),
            )
            .await?;
            Ok(back_to("/jobs", undo))
        }
        _ => Err(Failure::NotFound),
    }
}

/// A reader may undo its own changes: they were in its own folder.
fn op_in_own_folder(op: &Op, me: &Viewer) -> bool {
    op.account == Some(me.id)
}

/// `GET /jobs/events`: the jobs' state as server-sent events, one JSON
/// event per change that moved, every half second while the page is open.
pub async fn job_events(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
) -> Result<Response, Failure> {
    let (tx, rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(4);
    tokio::spawn(async move {
        let mut last: HashMap<i64, String> = HashMap::new();
        // An hour at most; the browser reconnects.
        for _ in 0..7200 {
            if let Ok(ops) = visible_ops(&state, &me).await {
                // The first look only learns where things stand: the page
                // already shows it.
                let first = last.is_empty();
                for (o, s) in ops {
                    let Some(s) = s else { continue };
                    let event = json!({ "id": o.id, "state": s.state, "done": s.done, "total": s.total, "message": s.message }).to_string();
                    if last.get(&o.id) == Some(&event) {
                        continue;
                    }
                    if !first
                        && tx
                            .send(bytes::Bytes::from(format!("data: {event}\n\n")))
                            .await
                            .is_err()
                    {
                        return;
                    }
                    last.insert(o.id, event);
                }
            }
            if tx.send(bytes::Bytes::from_static(b": \n\n")).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    Ok((
        [
            (CONTENT_TYPE, "text/event-stream"),
            (CACHE_CONTROL, "no-store"),
        ],
        axum::body::Body::new(Stream(rx)),
    )
        .into_response())
}

/// A response body fed from a channel.
struct Stream(tokio::sync::mpsc::Receiver<bytes::Bytes>);

impl http_body::Body for Stream {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        self.0
            .poll_recv(cx)
            .map(|b| b.map(|b| Ok(http_body::Frame::data(b))))
    }
}

/// The curator pages' script: the jobs page live, selection by runs.
pub const CURATOR_JS: &str = include_str!("../assets/curator/curator.js");

static CURATOR_JS_URL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let hex = blake3::hash(CURATOR_JS.as_bytes()).to_hex();
    format!("/a/curator.{}.js", &hex[..16])
});

pub fn curator_js_url() -> &'static str {
    &CURATOR_JS_URL
}
