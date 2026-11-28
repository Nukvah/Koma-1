//! The naming pages (docs/design: rename, fixup): a series' or a library's
//! rename plan as a dry run, applied as one journaled change, and the
//! fix-up queue of what cannot be named safely. See [`crate::naming`].

use std::collections::HashSet;
use std::fmt::Write;

use axum::Extension;
use axum::extract::{RawQuery, State};
use axum::response::{Html, IntoResponse, Response};
use serde_json::json;

use crate::auth::{Viewer, field, form, see_other};
use crate::curate::{
    all_ids, back_to, here, libraries, list_names, may_organise, query_all, query_ids, queue,
    refuse,
};
use crate::html::{escape, shell};
use crate::libraries::{Kind as LibraryKind, Library, library_of};
use crate::naming::{self, Row, Status};
use crate::serve::{AppState, Failure};

/// What a plan is of: one series, or one library.
#[derive(Clone)]
enum Scope {
    Series(i64),
    Library(String),
}

impl Scope {
    fn from(get: &dyn Fn(&str) -> Vec<String>) -> Option<Self> {
        if let Some(s) = get("s").first().and_then(|s| s.parse().ok()) {
            return Some(Scope::Series(s));
        }
        get("l").into_iter().next().map(Scope::Library)
    }

    fn query(&self) -> String {
        match self {
            Scope::Series(s) => format!("s={s}"),
            Scope::Library(l) => format!("l={}", crate::libraries::encode_path_part(l)),
        }
    }
}

/// Names kept as they are: by the worker (a tidy undone) and by curators.
async fn kept(state: &AppState) -> Result<HashSet<String>, Failure> {
    let mut kept: HashSet<String> = state
        .accounts(|a| Ok(a.kept_names()?))
        .await?
        .into_iter()
        .collect();
    kept.extend(state.query(|c| Ok(c.kept_names()?)).await?);
    Ok(kept)
}

/// A plan: its title, whether it is a cosplay library's tidy, the rows,
/// and the libraries for the side panel.
struct Plan {
    title: String,
    back: String,
    cosplay: bool,
    rows: Vec<Row>,
    libraries: Vec<Library>,
}

async fn plan(state: &AppState, me: &Viewer, scope: Scope) -> Result<Plan, Failure> {
    let kept = kept(state).await?;
    let viewer = me.clone();
    let root = state.library_root();
    state
        .query(move |c| {
            let libraries = libraries(c, &viewer)?;
            let (folder, title, back) = match &scope {
                Scope::Series(id) => {
                    let s = c
                        .series_by_id(*id)?
                        .filter(|s| viewer.sees(&s.path))
                        .ok_or(Failure::NotFound)?;
                    (
                        library_of(&s.path).to_owned(),
                        s.name.clone(),
                        format!("/s/{id}"),
                    )
                }
                Scope::Library(l) => {
                    let lib = libraries
                        .iter()
                        .find(|x| &x.folder == l)
                        .ok_or(Failure::NotFound)?;
                    (l.clone(), format!("all of {}", lib.name), lib.url())
                }
            };
            if !may_organise(&viewer, &folder) {
                return Err(refuse(&viewer));
            }
            let lib = libraries.iter().find(|l| l.folder == folder);
            // A cosplay library: the tidy, by the folder its sets are in.
            let top = folder.split('/').next().unwrap_or_default().to_owned();
            let cosplay = lib.is_some_and(|l| l.kind == LibraryKind::Cosplay)
                || naming::cosplay_libraries(c)?.contains(&top);
            let rows = if cosplay {
                naming::tidy_rows(c, &root, &top, &kept)?
                    .into_iter()
                    .filter(|r| {
                        matches!(&scope, Scope::Library(_))
                            || matches!(scope, Scope::Series(id) if r.series == id)
                    })
                    .collect()
            } else {
                match &scope {
                    Scope::Series(id) => {
                        let mut rows = naming::series_rows(c, *id, &kept)?;
                        naming::mark_clashes(&mut rows, &root);
                        rows
                    }
                    Scope::Library(l) => naming::library_rows(c, &root, l, &kept)?,
                }
            };
            Ok(Plan {
                title,
                back,
                cosplay,
                rows,
                libraries,
            })
        })
        .await
}

/// `GET /naming?s=<series>` or `?l=<library>`: the rename plan.
pub async fn naming_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    RawQuery(q): RawQuery,
) -> Result<Response, Failure> {
    let get = |k: &str| query_all(q.as_deref(), k);
    let scope = Scope::from(&get).ok_or(Failure::NotFound)?;
    let p = plan(&state, &me, scope.clone()).await?;
    let renames = p.rows.iter().filter(|r| r.status == Status::Rename).count();
    let same = p.rows.iter().filter(|r| r.status == Status::Same).count();
    let fixups = p.rows.len() - renames - same;
    let mut m = String::new();
    let _ = write!(
        m,
        "<div class=toolbar><h1>naming · <b>{}</b></h1><a href=\"{}\">back</a></div><div class=pair><section class=box>",
        escape(&p.title),
        escape(&p.back)
    );
    if p.cosplay {
        m.push_str(
            "<h2>a cosplay library · kept tidy by itself</h2><div class=status><p>After each scan the worker \
             keeps every set in <code>Cosplayer / Set /</code> folders, keeping its own name, as a journaled \
             change on <a href=/jobs>jobs</a> you can undo. Sets with no cosplayer in their name stay where \
             they are, in the fix-up queue.</p></div>",
        );
    } else {
        m.push_str(
            "<h2>the name a book gets · manga and comics</h2><p><code>(Event) [Circle (Author)] Series - Ch. 003 [EN].cbz</code></p>\
             <div class=status><p>Each part only when known: from the book's details (its metadata, your edits), else \
             the series' other books. A volume is <code>Vol. 2</code>.</p><p>Numbers are padded to the series' largest: \
             <code>Ch. 3</code> in a series that stops at 9, <code>Ch. 003</code> in one that reaches 120.</p>\
             <p>Only the file name changes; the folder is the series. Everything Koma-1 reads from a name is in the new one.</p></div>",
        );
    }
    let _ = write!(
        m,
        "</section><section class=box><h2>this plan</h2><div class=status><p><b>{renames}</b> to {} · {same} already right · \
         <span class=pending>{fixups} for the fix-up queue</span></p><p>A dry run: nothing changes until it is applied. \
         Applying it is one journaled change, with undo; books keep their ids, progress and edits.</p></div></section></div>",
        if p.cosplay {
            "tidy (by the worker, at its next scan)"
        } else {
            "rename"
        }
    );
    let (key, value) = match &scope {
        Scope::Series(s) => ("s", s.to_string()),
        Scope::Library(l) => ("l", l.clone()),
    };
    let _ = write!(
        m,
        "<h2 class=section-h>plan · dry run</h2><form method=post action=/naming>\
         <input type=hidden name={key} value=\"{}\">",
        escape(&value)
    );
    m.push_str("<div class=scroll><table class=plan><tbody>");
    for r in &p.rows {
        match &r.status {
            Status::Same => {}
            Status::Rename => {
                let _ = write!(
                    m,
                    "<tr><td>{}</td><td><div class=from>{}</div><div class=to>{}</div></td><td></td></tr>",
                    if p.cosplay {
                        "<span class=hint>tidy</span>".to_owned()
                    } else {
                        format!(
                            "<label class=hint><input type=checkbox name=b value={} checked> rename</label>",
                            r.book
                        )
                    },
                    escape(&r.from),
                    escape(&r.to)
                );
            }
            Status::Fixup(why) => {
                let _ = write!(
                    m,
                    "<tr class=warn><td><span class=pending>fix-up</span></td><td><div class=to>{}</div><div class=hint>{}</div></td>\
                     <td><a href=\"/fixup?b={}\">fix…</a></td></tr>",
                    escape(&r.from),
                    escape(why),
                    r.book
                );
            }
        }
    }
    if renames + fixups == 0 {
        m.push_str("<tr><td>Every name is right already.</td></tr>");
    }
    m.push_str("</tbody></table></div>");
    if !p.cosplay && renames > 0 {
        let _ = write!(
            m,
            "<div class=actbar><button class=\"btn primary\" type=submit>rename {renames}</button>"
        );
    } else {
        m.push_str("<div class=actbar>");
    }
    if fixups > 0 {
        let _ = write!(
            m,
            "<a class=btn href=\"/fixup?{}\">fix-up queue · {fixups}</a>",
            scope.query()
        );
    }
    let _ = write!(
        m,
        "<a class=btn href=\"{}\">cancel</a></div></form>",
        escape(&p.back)
    );
    let me2 = me.me();
    Ok(Html(shell("Naming", &p.libraries, &here(&me2, "library"), &m)).into_response())
}

/// `POST /naming`: renames the books ticked (`b`) of the plan of `s` or `l`.
pub async fn naming_apply(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let get = |k: &str| crate::curate::all(&f, k);
    let scope = Scope::from(&get).ok_or(Failure::NotFound)?;
    let ticked: HashSet<i64> = all_ids(&f, "b").into_iter().collect();
    let p = plan(&state, &me, scope).await?;
    if p.cosplay {
        return Ok(see_other(&p.back));
    }
    let chosen: Vec<&Row> = p
        .rows
        .iter()
        .filter(|r| r.status == Status::Rename && ticked.contains(&r.book))
        .collect();
    if chosen.is_empty() {
        return Ok(see_other(&p.back));
    }
    let books: Vec<serde_json::Value> = chosen
        .iter()
        .map(|r| json!({ "id": r.book, "to": r.to.rsplit('/').next().unwrap_or(&r.to) }))
        .collect();
    let title = format!(
        "Rename {} of {} to the naming standard",
        crate::html::plural(chosen.len() as i64, "book", "books"),
        p.title.trim_start_matches("all of ")
    );
    let (op, _) = queue(&state, &me, "rename", json!({ "books": books }), title).await?;
    Ok(back_to(&p.back, op))
}

/// `GET /fixup` (`?b=`, `?s=` or `?l=` to narrow it): what cannot be named
/// safely, in every library the viewer organises.
pub async fn fixup_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    RawQuery(q): RawQuery,
) -> Result<Response, Failure> {
    let only: HashSet<i64> = query_ids(q.as_deref(), "b").into_iter().collect();
    let get = |k: &str| query_all(q.as_deref(), k);
    let scope = Scope::from(&get);
    let kept = kept(&state).await?;
    let viewer = me.clone();
    let root = state.library_root();
    let (libraries, rows) = state
        .query(move |c| {
            let libraries = libraries(c, &viewer)?;
            let cosplay = naming::cosplay_libraries(c)?;
            let mut rows = Vec::new();
            let mut seen_cosplay = HashSet::new();
            for l in libraries
                .iter()
                .filter(|l| may_organise(&viewer, &l.folder))
            {
                if let Some(Scope::Library(want)) = &scope
                    && want != &l.folder
                {
                    continue;
                }
                let top = l.folder.split('/').next().unwrap_or_default().to_owned();
                if l.kind == LibraryKind::Cosplay || cosplay.contains(&top) {
                    if seen_cosplay.insert(top.clone()) {
                        rows.extend(naming::tidy_rows(c, &root, &top, &kept)?);
                    }
                } else {
                    rows.extend(naming::library_rows(c, &root, &l.folder, &kept)?);
                }
            }
            // A cosplay library folder whose sets are not in a library of
            // their own yet (straight in the folder).
            for top in cosplay {
                if may_organise(&viewer, &top)
                    && viewer.sees(&format!("{top}/x"))
                    && seen_cosplay.insert(top.clone())
                {
                    rows.extend(naming::tidy_rows(c, &root, &top, &kept)?);
                }
            }
            rows.retain(|r| matches!(r.status, Status::Fixup(_)));
            if let Some(Scope::Series(s)) = scope {
                rows.retain(|r| r.series == s);
            }
            if !only.is_empty() {
                rows.retain(|r| only.contains(&r.book));
            }
            Ok((libraries, rows))
        })
        .await?;
    let mut m = String::new();
    let _ = write!(
        m,
        "<div class=toolbar><h1>naming · <b>fix-up queue · {}</b></h1></div>\
         <p class=note>Files Koma-1 cannot name by itself. Fill what it could not read; the new name is shown \
         when you rename. “Keep the name it has” takes a file out of the queue.</p>",
        rows.len()
    );
    if rows.is_empty() {
        m.push_str("<div class=empty><strong>Nothing to fix</strong><p>Every file can be named by itself.</p></div>");
    }
    for r in &rows {
        let Status::Fixup(why) = &r.status else {
            continue;
        };
        let _ = write!(
            m,
            "<form class=\"box form\" method=post action=/fixup><div><b>{}</b><br><span class=pending>{}</span></div>\
             <input type=hidden name=b value={}><div class=row>",
            escape(&r.from),
            escape(why),
            r.book
        );
        let input = |m: &mut String, name: &str, label: &str, value: &str, size: u32| {
            let _ = write!(
                m,
                "<label>{label}<input name={name} value=\"{}\" size={size}></label>",
                escape(value)
            );
        };
        let cosplay = r.cosplay;
        if cosplay {
            let file = r.from.rsplit('/').next().unwrap_or(&r.from);
            let stem = if r.photos {
                file
            } else {
                file.rsplit_once('.').map_or(file, |(s, _)| s)
            };
            let n = crate::names::cosplay_name(stem);
            input(&mut m, "cosplayer", "cosplayer", &n.cosplayer, 18);
            input(&mut m, "set", "set", &n.set, 18);
            input(&mut m, "variant", "variant", &n.variant, 12);
            input(&mut m, "circle", "circle", &n.circle, 14);
        } else {
            let _ = write!(
                m,
                "<label>series<input value=\"{}\" disabled size=18></label>",
                escape(&r.parts.series)
            );
            let _ = write!(m, "<label>kind<select name=kind>");
            for (v, label) in [("", "—"), ("chapter", "chapter"), ("volume", "volume")] {
                let on = match r.parts.kind {
                    Some(crate::names::Kind::Chapter) => v == "chapter",
                    Some(crate::names::Kind::Volume) => v == "volume",
                    None => v.is_empty(),
                };
                let _ = write!(
                    m,
                    "<option value=\"{v}\"{}>{label}</option>",
                    if on { " selected" } else { "" }
                );
            }
            m.push_str("</select></label>");
            input(
                &mut m,
                "number",
                "number",
                &r.parts.number.map(|n| n.to_string()).unwrap_or_default(),
                6,
            );
            input(
                &mut m,
                "author",
                "author",
                r.parts.author.as_deref().unwrap_or_default(),
                16,
            );
            input(
                &mut m,
                "circle",
                "circle",
                r.parts.circle.as_deref().unwrap_or_default(),
                16,
            );
        }
        let _ = write!(
            m,
            "<input type=hidden name=kind_of value={}></div><div class=row><button class=\"btn primary\" type=submit name=action value=rename>rename</button>\
             <button class=btn type=submit name=action value=keep>keep the name it has</button></div></form>",
            if cosplay { "cosplay" } else { "manga" }
        );
    }
    let me2 = me.me();
    Ok(Html(shell("Fix-up", &libraries, &here(&me2, "library"), &m)).into_response())
}

/// `POST /fixup`: one file: `keep` its name, or `rename` it from the
/// fields (manga: kind, number, author, circle, kept as its details too;
/// cosplay: cosplayer, set, variant, circle, which the tidy then places).
pub async fn fixup_apply(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let book = field(&f, "b")
        .parse::<i64>()
        .map_err(|_| Failure::NotFound)?;
    let viewer = me.clone();
    let (path, series, photos, name) = state
        .query(move |c| {
            let b = c.book(book)?.ok_or(Failure::NotFound)?;
            let s = c.series_by_id(b.series_id)?.ok_or(Failure::NotFound)?;
            if !viewer.sees(&s.path) {
                return Err(Failure::NotFound);
            }
            if !may_organise(&viewer, library_of(&s.path))
                && !may_organise(&viewer, b.path.split('/').next().unwrap_or_default())
            {
                return Err(refuse(&viewer));
            }
            Ok((b.path, s, b.photos, b.name))
        })
        .await?;
    if field(&f, "action") == "keep" {
        let (id, p) = (me.id, path.clone());
        state.accounts(move |a| Ok(a.keep_name(id, &p)?)).await?;
        return Ok(see_other("/fixup"));
    }
    let file = path.rsplit('/').next().unwrap_or(&path).to_owned();
    let ext = if photos {
        None
    } else {
        file.rsplit_once('.').map(|(_, e)| e.to_lowercase())
    };
    let text = |k: &str| {
        field(&f, k)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let stem = if field(&f, "kind_of") == "cosplay" {
        let (who, set, variant, circle) = (
            text("cosplayer"),
            text("set"),
            text("variant"),
            text("circle"),
        );
        if who.is_empty() || set.is_empty() {
            return Err(Failure::Forbidden("A cosplayer and a set, please.".into()));
        }
        let mut s = String::new();
        if !circle.is_empty() {
            s.push_str(&format!("[{circle}] "));
        }
        s.push_str(&format!("{who} - {set}"));
        if !variant.is_empty() {
            s.push_str(&format!(" - {variant}"));
        }
        s
    } else {
        let kind = match field(&f, "kind") {
            "chapter" => Some(crate::names::Kind::Chapter),
            "volume" => Some(crate::names::Kind::Volume),
            _ => None,
        };
        let number = field(&f, "number")
            .replace(',', ".")
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0);
        let (author, circle) = (text("author"), text("circle"));
        // The details first, so the name and the catalogue agree.
        let mut set = serde_json::Map::new();
        set.insert(
            "number".into(),
            number.map_or(serde_json::Value::Null, |n| json!(n)),
        );
        set.insert(
            "kind".into(),
            kind.map_or(serde_json::Value::Null, |k| json!(k.as_str())),
        );
        if !author.is_empty() {
            set.insert("authors".into(), json!([author]));
        }
        if !circle.is_empty() {
            set.insert("publishers".into(), json!([circle]));
        }
        queue(
            &state,
            &me,
            "details",
            json!({ "books": [book], "set": set }),
            format!("Details of {name}"),
        )
        .await?;
        let sid = series.id;
        let kept = kept(&state).await?;
        let widths = state
            .query(move |c| {
                let rows = naming::series_rows(c, sid, &kept)?;
                Ok(naming::Widths::of(
                    rows.iter()
                        .filter_map(|r| Some((r.parts.kind, r.parts.number?))),
                ))
            })
            .await?;
        let parts = naming::Parts {
            event: naming::event_of(&name),
            circle: (!circle.is_empty()).then_some(circle),
            author: (!author.is_empty()).then_some(author),
            series: series.name.clone(),
            kind,
            number,
            language: naming::language_of(&name),
        };
        naming::build(&parts, widths)
    };
    let to = match ext {
        Some(e) => format!("{stem}.{e}"),
        None => stem,
    };
    if !crate::organise::safe_name(&to) {
        return Err(Failure::Forbidden(format!("“{to}” can't be a file name.")));
    }
    let title = format!("Rename {}", list_names(&[file.as_str()]));
    let (op, _) = queue(
        &state,
        &me,
        "rename",
        json!({ "books": [{ "id": book, "to": to }] }),
        title,
    )
    .await?;
    Ok(back_to("/fixup", op))
}
