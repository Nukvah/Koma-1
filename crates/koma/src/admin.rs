//! The account page, user management (the admin's), library settings and
//! what an account keeps: the requests that write accounts.db. None of
//! them touch the catalogue or `/library`; a library setting is queued
//! for the worker.

use axum::Extension;
use axum::extract::{RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};

use crate::accounts::{Role, SettingChange};
use crate::auth::{Viewer, admin_only, field, form, see_other};
use crate::libraries::{Direction, Kind, Library, colour};
use crate::serve::{AppState, Failure, query_value};
use crate::{b64, kept, pages, password};

/// Every library (with the settings changed in the UI); none before the
/// first scan.
async fn all_libraries(state: &AppState) -> Result<Vec<Library>, Failure> {
    match state
        .query(|c| Ok(Library::all(c.library_keys()?, &c.library_settings()?)))
        .await
    {
        Err(Failure::NotScanned) => Ok(Vec::new()),
        other => other,
    }
}

fn visible(all: &[Library], me: &Viewer) -> Vec<Library> {
    all.iter()
        .filter(|l| l.visible_to(&me.name))
        .cloned()
        .collect()
}

/// Libraries and series in `name`'s own folder.
fn own(all: &[Library], name: &str) -> (usize, i64) {
    all.iter()
        .filter(|l| l.owner() == Some(name))
        .fold((0, 0), |(n, s), l| (n + 1, s + l.series))
}

// The account page.

/// `GET /account`; `?done=` says what was just done.
pub async fn account_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    RawQuery(q): RawQuery,
) -> Result<Response, Failure> {
    let note = match query_value(q.as_deref(), "done").as_deref() {
        Some("password") => Some("Password changed. Your other devices are signed out."),
        Some("signout") => Some("Signed out there."),
        Some("others") => Some("Signed out everywhere else."),
        Some("passkey") => Some("Passkeys updated."),
        _ => None,
    };
    account_with(&state, &me, note, None, StatusCode::OK).await
}

async fn account_with(
    state: &AppState,
    me: &Viewer,
    note: Option<&str>,
    error: Option<&str>,
    status: StatusCode,
) -> Result<Response, Failure> {
    let all = all_libraries(state).await?;
    let id = me.id;
    let (account, sessions, passkeys, positions, series_settings) = state
        .accounts(move |a| {
            let account = a.by_id(id)?.ok_or(Failure::NotFound)?;
            let positions = a.kept_count(id, "pos:")? + a.kept_count(id, "epos:")?;
            Ok((
                account,
                a.sessions(id)?,
                a.passkeys(id)?,
                positions,
                a.kept_series(id)?,
            ))
        })
        .await?;
    let html = pages::account(
        &me.me(),
        &visible(&all, me),
        &pages::AccountView {
            account: &account,
            sessions: &sessions,
            current: me.session,
            passkeys: &passkeys,
            positions,
            series_settings,
            own: own(&all, &me.name),
            note,
            error,
        },
    );
    Ok((status, Html(html)).into_response())
}

/// `POST /account/password`: current, password, again.
pub async fn account_password(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let (current, new) = (
        field(&f, "current").to_owned(),
        field(&f, "password").to_owned(),
    );
    if let Err(e) = password::check_new(&new, field(&f, "again")) {
        return account_with(&state, &me, None, Some(e), StatusCode::BAD_REQUEST).await;
    }
    let (id, session) = (me.id, me.session);
    let ok = state
        .accounts(move |a| {
            let account = a.by_id(id)?.ok_or(Failure::NotFound)?;
            if !account
                .password
                .as_deref()
                .is_some_and(|p| password::verify(&current, p))
            {
                return Ok(false);
            }
            a.set_password(id, &password::hash(&new), session.as_ref())?;
            Ok(true)
        })
        .await?;
    if ok {
        Ok(see_other("/account?done=password"))
    } else {
        account_with(
            &state,
            &me,
            None,
            Some("Your current password is not that."),
            StatusCode::BAD_REQUEST,
        )
        .await
    }
}

/// `POST /account/signout`: `s` is a session's short id, or `others`.
pub async fn account_signout(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let which = field(&f, "s").to_owned();
    let (id, mine) = (me.id, me.session);
    let done = state
        .accounts(move |a| {
            if which == "others" {
                if let Some(h) = mine {
                    a.end_other_sessions(id, &h)?;
                }
                return Ok("others");
            }
            for s in a.sessions(id)? {
                if blake3::Hash::from(s.hash).to_hex().starts_with(&which) && which.len() == 16 {
                    a.end_session(id, &s.hash)?;
                }
            }
            Ok("signout")
        })
        .await?;
    Ok(see_other(&format!("/account?done={done}")))
}

/// `POST /account/passkey`: id, do (rename or remove), name.
pub async fn account_passkey(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let id = b64::decode(field(&f, "id")).ok_or(Failure::NotFound)?;
    let action = field(&f, "do").to_owned();
    let name: String = field(&f, "name")
        .chars()
        .filter(|c| !c.is_control())
        .take(40)
        .collect();
    let account = me.id;
    state
        .accounts(move |a| {
            match action.as_str() {
                "remove" => a.remove_passkey(account, &id)?,
                "rename" if !name.trim().is_empty() => {
                    a.rename_passkey(account, &id, name.trim())?
                }
                _ => false,
            };
            Ok(())
        })
        .await?;
    Ok(see_other("/account?done=passkey"))
}

// Users (the admin's).

/// `GET /users`.
pub async fn users_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
) -> Result<Response, Failure> {
    users_with(&state, &me, None, None, StatusCode::OK).await
}

async fn users_with(
    state: &AppState,
    me: &Viewer,
    made: Option<(String, String)>,
    error: Option<&str>,
    status: StatusCode,
) -> Result<Response, Failure> {
    admin_only(me, "manage accounts")?;
    let all = all_libraries(state).await?;
    let rows = state
        .accounts(|a| {
            let mut rows = Vec::new();
            for account in a.all()? {
                rows.push(pages::UserRow {
                    passkeys: a.passkeys(account.id)?.len(),
                    link: a.has_link(account.id)?,
                    series: 0,
                    account,
                });
            }
            Ok(rows)
        })
        .await?;
    let rows: Vec<pages::UserRow> = rows
        .into_iter()
        .map(|mut r| {
            r.series = own(&all, &r.account.name).1;
            r
        })
        .collect();
    let html = pages::users(
        &me.me(),
        &visible(&all, me),
        &rows,
        made.as_ref().map(|(n, u)| (n.as_str(), u.as_str())),
        error,
        state.auth(),
    );
    Ok((status, Html(html)).into_response())
}

/// `POST /users/add`: name, role.
pub async fn users_add(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    admin_only(&me, "add accounts")?;
    let f = form(&body);
    let name = field(&f, "name").trim().to_lowercase();
    let role = Role::parse(field(&f, "role"))
        .filter(|r| *r != Role::Admin)
        .unwrap_or(Role::Reader);
    let n = name.clone();
    let made = state
        .accounts(move |a| match a.create(&n, role, None) {
            Ok(account) => Ok(Ok(a.new_link(account.id)?)),
            Err(crate::accounts::CreateError::Taken) => Ok(Err("There is already an account of that name.")),
            Err(crate::accounts::CreateError::BadName) => Ok(Err(
                "A name is lower-case letters, digits, “.”, “_” and “-”, at most 32, starting with a letter or digit.",
            )),
            Err(crate::accounts::CreateError::Db(e)) => Err(Failure::Internal(e)),
        })
        .await?;
    match made {
        Ok(token) => {
            crate::log::info("auth: account added", &[("name", &name), ("by", &me.name)]);
            let url = format!(
                "{}/join/{}",
                crate::auth::site(&headers),
                b64::encode(&token)
            );
            users_with(&state, &me, Some((name, url)), None, StatusCode::OK).await
        }
        Err(e) => users_with(&state, &me, None, Some(e), StatusCode::BAD_REQUEST).await,
    }
}

/// `POST /users/role`: name, role (reader or curator).
pub async fn users_role(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    admin_only(&me, "set roles")?;
    let f = form(&body);
    let name = field(&f, "name").to_owned();
    let role = Role::parse(field(&f, "role")).ok_or(Failure::NotFound)?;
    state
        .accounts(move |a| {
            if let Some(account) = a.by_name(&name)? {
                a.set_role(account.id, role)?;
            }
            Ok(())
        })
        .await?;
    Ok(see_other("/users"))
}

/// `POST /users/link`: name. A new sign-in link (also to reset a
/// forgotten password).
pub async fn users_link(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    admin_only(&me, "make sign-in links")?;
    let f = form(&body);
    let name = field(&f, "name").to_owned();
    let n = name.clone();
    let token = state
        .accounts(move |a| match a.by_name(&n)? {
            Some(account) if account.role != Role::Admin => Ok(Some(a.new_link(account.id)?)),
            _ => Ok(None),
        })
        .await?
        .ok_or(Failure::NotFound)?;
    let url = format!(
        "{}/join/{}",
        crate::auth::site(&headers),
        b64::encode(&token)
    );
    users_with(&state, &me, Some((name, url)), None, StatusCode::OK).await
}

/// `POST /users/remove`: name. Its folder stays on disk.
pub async fn users_remove(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    admin_only(&me, "remove accounts")?;
    let f = form(&body);
    let name = field(&f, "name").to_owned();
    let n = name.clone();
    let removed = state
        .accounts(move |a| match a.by_name(&n)? {
            Some(account) => Ok(a.remove(account.id)?),
            None => Ok(false),
        })
        .await?;
    if removed {
        crate::log::info(
            "auth: account removed",
            &[("name", &name), ("by", &me.name)],
        );
    }
    Ok(see_other("/users"))
}

/// `POST /users/handover`: to, password (the admin's own).
pub async fn users_handover(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    admin_only(&me, "hand over the admin role")?;
    let f = form(&body);
    let to = field(&f, "to").to_owned();
    let pw = field(&f, "password").to_owned();
    let id = me.id;
    let done = state
        .accounts(move |a| {
            let admin = a.by_id(id)?.ok_or(Failure::NotFound)?;
            if !admin
                .password
                .as_deref()
                .is_some_and(|p| password::verify(&pw, p))
            {
                return Ok(Err("That is not your password."));
            }
            let Some(target) = a.by_name(&to)? else {
                return Ok(Err("There is no such account."));
            };
            Ok(if a.hand_over(id, target.id)? {
                crate::log::info(
                    "auth: admin role handed over",
                    &[("from", &admin.name), ("to", &to)],
                );
                Ok(())
            } else {
                Err("The role could not be handed over.")
            })
        })
        .await?;
    match done {
        Ok(()) => Ok(see_other("/account")),
        Err(e) => users_with(&state, &me, None, Some(e), StatusCode::BAD_REQUEST).await,
    }
}

// Library settings.

/// Whether `me` may change `library`'s settings: the admin the shared
/// ones, each account those in its own folder.
fn may_edit(me: &Viewer, library: &Library) -> bool {
    match library.owner() {
        None => me.is_admin(),
        Some(owner) => owner == me.name,
    }
}

/// `GET /settings`.
pub async fn settings_page(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
) -> Result<Response, Failure> {
    settings_with(&state, &me, None, StatusCode::OK).await
}

async fn settings_with(
    state: &AppState,
    me: &Viewer,
    error: Option<&str>,
    status: StatusCode,
) -> Result<Response, Failure> {
    let (all, applied) = match state
        .query(|c| {
            Ok((
                Library::all(c.library_keys()?, &c.library_settings()?),
                c.settings_applied()?,
            ))
        })
        .await
    {
        Err(Failure::NotScanned) => (Vec::new(), 0),
        other => other?,
    };
    let libraries = visible(&all, me);
    let queued: Vec<String> = state
        .accounts(move |a| Ok(a.changes_after(applied)?))
        .await?
        .into_iter()
        .map(|c| c.folder)
        .filter(|f| libraries.iter().any(|l| &l.folder == f))
        .collect();
    let me2 = me.clone();
    let html = pages::settings(
        &me.me(),
        &libraries,
        &move |l| may_edit(&me2, l),
        &queued,
        error,
    );
    Ok((status, Html(html)).into_response())
}

/// `POST /settings`: folder, name, kind, dir, colour, widths. Queued; the worker
/// writes it into the catalogue.
pub async fn settings_save(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let f = form(&body);
    let folder = field(&f, "folder").to_owned();
    let all = all_libraries(&state).await?;
    let library = all
        .iter()
        .find(|l| l.folder == folder && l.visible_to(&me.name))
        .ok_or(Failure::NotFound)?;
    if !may_edit(&me, library) {
        return Err(Failure::Forbidden(format!(
            "Only the admin can change a shared library's settings. You are signed in as {}, a {}.",
            me.name,
            me.role.as_str()
        )));
    }
    let name = field(&f, "name").trim().to_owned();
    // A form from before the setting keeps what the library has.
    let widths = match field(&f, "widths") {
        "" => Some(library.widths),
        w => w
            .parse::<u8>()
            .ok()
            .filter(|w| crate::libraries::WIDTH_CHOICES.iter().any(|c| c.0 == *w)),
    };
    let change = match (
        Kind::parse(field(&f, "kind")),
        Direction::from_code(field(&f, "dir")),
        colour(field(&f, "colour")),
        widths,
    ) {
        (Some(kind), Some(dir), Some(c), Some(widths))
            if !name.is_empty()
                && name.chars().count() <= 64
                && !name.chars().any(char::is_control) =>
        {
            SettingChange {
                id: 0,
                folder,
                name,
                kind: kind.as_str().to_owned(),
                direction: dir.code().to_owned(),
                colour: c.0.to_owned(),
                widths,
            }
        }
        _ => {
            return settings_with(
                &state,
                &me,
                Some("A name (at most 64 characters), a type, a direction, a colour and page copies, please."),
                StatusCode::BAD_REQUEST,
            )
            .await;
        }
    };
    let id = me.id;
    state
        .accounts(move |a| Ok(a.queue_change(&change, id)?))
        .await?;
    Ok(see_other("/settings"))
}

// What an account keeps.

/// `POST /kept`: form fields `key=value`, from the readers' scripts
/// (`sendBeacon`). Keys and values are checked ([`kept::allowed`]).
pub async fn kept_save(
    State(state): State<AppState>,
    Extension(me): Extension<Viewer>,
    body: axum::body::Bytes,
) -> Result<Response, Failure> {
    let pairs = kept::only_allowed(form(&body));
    if !pairs.is_empty() {
        let id = me.id;
        state
            .accounts(move |a| Ok(a.keep(id, &pairs, false)?))
            .await?;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
