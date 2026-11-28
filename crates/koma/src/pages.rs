//! Account pages (docs/design: signin, setup, join, account, users,
//! settings): server-rendered like the library, in the same shell, with
//! one small script for passkeys ([`ACCOUNT_JS`]). Every form works without
//! it; passkeys need it.

use std::fmt::Write;
use std::sync::LazyLock;

use crate::accounts::{Account, Passkey, Role, Session};
use crate::html::{Here, css_url, escape, shell};
use crate::libraries::{COLOURS, Direction, Kind, Library, WIDTH_CHOICES};

/// Who is signed in, as pages show it.
pub struct Me<'a> {
    pub name: &'a str,
    pub role: Role,
}

/// The passkey script: signing in with one, adding one, and bringing what
/// this browser kept before accounts along with the sign-in form.
pub const ACCOUNT_JS: &str = include_str!("../assets/account/account.js");

static ACCOUNT_JS_URL: LazyLock<String> = LazyLock::new(|| {
    let hex = blake3::hash(ACCOUNT_JS.as_bytes()).to_hex();
    format!("/a/account.{}.js", &hex[..16])
});

pub fn account_js_url() -> &'static str {
    &ACCOUNT_JS_URL
}

fn role_tag(role: Role) -> String {
    format!(
        "<span class=\"role{}\">{}</span>",
        if role == Role::Admin { " admin" } else { "" },
        role.as_str()
    )
}

/// The top bar's user menu: a `<details>`, no script.
pub fn user_menu(me: &Me<'_>) -> String {
    let initial: String = me
        .name
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_default();
    format!(
        "<details class=umenu><summary><span class=avatar aria-hidden=true>{}</span>\
         <span>{} ▾</span></summary><ul><li><span><small>signed in as</small> {} {}</span></li>\
         <li><hr></li><li><a href=/account>account</a></li>{}<li><a href=/settings>libraries</a></li>\
         <li><hr></li><li><form method=post action=/signout><button type=submit>sign out</button>\
         </form></li></ul></details>",
        escape(&initial),
        escape(me.name),
        escape(me.name),
        role_tag(me.role),
        if me.role == Role::Admin {
            "<li><a href=/users>users</a></li>"
        } else {
            ""
        }
    )
}

/// A page outside the shell: the brand and one box in the middle.
fn gate(title: &str, body: &str, script: bool) -> String {
    format!(
        "<!doctype html>\n<html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{} · Koma-1</title><link rel=stylesheet href=\"{}\">{}</head><body>\
         <header class=top><a class=brand href=/>KOMA-1</a></header><main class=gate>{body}</main>\
         </body></html>\n",
        escape(title),
        css_url(),
        if script {
            format!("<script defer src=\"{}\"></script>", account_js_url())
        } else {
            String::new()
        }
    )
}

fn error_note(error: Option<&str>) -> String {
    error.map_or_else(String::new, |e| {
        format!("<p class=\"note err\" role=alert>{}</p>", escape(e))
    })
}

/// `/signin`.
pub fn signin(error: Option<&str>, name: &str, next: &str) -> String {
    let body = format!(
        "<form class=\"box form\" method=post action=/signin data-kept><strong>Sign in</strong>{}\
         <label>name<input name=name value=\"{}\" autocomplete=\"username webauthn\" required autofocus \
         autocapitalize=none></label>\
         <label>password<input name=password type=password autocomplete=current-password required></label>\
         <input type=hidden name=next value=\"{}\"><input type=hidden name=kept value=\"\">\
         <div class=row><button class=\"btn primary\" type=submit>sign in</button>\
         <label class=check><input type=checkbox name=keep value=1 checked> keep me signed in on this device</label></div>\
         <div id=pk hidden><p class=or>or</p><button class=btn type=button id=pk-signin>sign in with a passkey</button></div>\
         <p id=pk-msg class=\"note err\" hidden></p>\
         <small>Forgot your password? Ask the admin for a new sign-in link.</small></form>",
        error_note(error),
        escape(name),
        escape(next)
    );
    gate("Sign in", &body, true)
}

/// `/setup`: the first account.
pub fn setup(error: Option<&str>, name: &str) -> String {
    let body = format!(
        "<form class=\"box form\" method=post action=/setup><span class=label>first start</span>\
         <strong>Create the admin account</strong>{}\
         <p class=hint>No account exists yet. The first one is the admin: it adds and removes the \
         others and edits library settings.</p>\
         <label>setup code<input name=code autocomplete=one-time-code required autocapitalize=characters>\
         <small>Printed in the log when <code>koma serve</code> starts (<code>docker logs koma</code>), \
         so only whoever runs the server can claim it.</small></label>\
         <label>name<input name=name value=\"{}\" autocomplete=username required autocapitalize=none \
         pattern=\"[a-z0-9][a-z0-9._\\-]{{0,31}}\"><small>Lower-case letters, digits, . _ -; \
         also your folder, <code>/library/~name</code>.</small></label>\
         <label>password<input name=password type=password autocomplete=new-password minlength=10 required>\
         <small>At least 10 characters.</small></label>\
         <label>password again<input name=again type=password autocomplete=new-password required></label>\
         <button class=\"btn primary\" type=submit>create and sign in</button></form>",
        error_note(error),
        escape(name)
    );
    gate("First start", &body, false)
}

/// `/join/<token>`: a sign-in link.
pub fn join(account: &Account, error: Option<&str>) -> String {
    let what = match account.role {
        Role::Reader => "a <b>reader</b>: you can read and download",
        Role::Curator => "a <b>curator</b>: you can read, download and organise the library",
        Role::Admin => "the <b>admin</b>",
    };
    let (title, lead) = if account.password.is_some() {
        (
            "Choose a new password",
            "Choose a new password to sign in; this link then stops working.".to_owned(),
        )
    } else {
        (
            "Welcome",
            format!(
                "Your account is {what}. Choose a password to finish; this link then stops working."
            ),
        )
    };
    let body = format!(
        "<form class=\"box form\" method=post><span class=label>sign-in link</span>\
         <strong>{title}, {}</strong>{}<p class=hint>{lead}</p>\
         <input type=hidden name=name value=\"{}\" autocomplete=username>\
         <label>password<input name=password type=password autocomplete=new-password minlength=10 required>\
         <small>At least 10 characters.</small></label>\
         <label>password again<input name=again type=password autocomplete=new-password required></label>\
         <button class=\"btn primary\" type=submit>set password and sign in</button>\
         <small>You can add a passkey afterwards, in your account.</small></form>",
        escape(&account.name),
        error_note(error),
        escape(&account.name),
    );
    gate(title, &body, false)
}

pub fn dead_link() -> String {
    gate(
        "Sign-in link",
        "<div class=\"box form\"><span class=label>link</span><strong>This sign-in link no longer works</strong>\
         <p class=hint>Each link works once, for 24 hours. Ask the admin for a new one.</p>\
         <a class=btn href=/signin>sign in</a></div>",
        false,
    )
}

/// A refusal (403): what was asked, and why not.
pub fn refusal(text: &str) -> String {
    gate(
        "Not allowed",
        &format!(
            "<div class=\"box form\"><span class=label>403</span><strong>Not allowed</strong>\
             <p class=hint>{}</p><a class=btn href=/>back to the library</a></div>",
            escape(text)
        ),
        false,
    )
}

/// `5 minutes ago`, `yesterday`, `2026-09-02`.
pub fn ago(t: i64, now: i64) -> String {
    let d = (now - t).max(0);
    match d {
        0..60 => "now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86_400 => {
            let h = d / 3600;
            format!("{h} hour{} ago", if h == 1 { "" } else { "s" })
        }
        86_400..172_800 => "yesterday".into(),
        172_800..2_592_000 => format!("{} days ago", d / 86_400),
        _ => crate::html::date(t),
    }
}

fn settings_tabs(m: &mut String, current: &str, me: &Me<'_>) {
    let _ = write!(
        m,
        "<div class=toolbar><h1>settings · <b>{current}</b></h1><nav class=seg aria-label=Settings>"
    );
    for (name, href) in [
        ("libraries", "/settings"),
        ("users", "/users"),
        ("account", "/account"),
    ] {
        if name == "users" && me.role != Role::Admin {
            continue;
        }
        let _ = write!(
            m,
            "<a href={href}{}>{name}</a>",
            if name == current {
                " aria-current=page"
            } else {
                ""
            }
        );
    }
    m.push_str("</nav></div>");
}

fn here<'a>(me: &'a Me<'a>) -> Here<'a> {
    Here {
        library: None,
        collections: &[],
        collection: None,
        go: "settings",
        query: "",
        scroll: false,
        me,
    }
}

/// What `/account` shows.
pub struct AccountView<'a> {
    pub account: &'a Account,
    pub sessions: &'a [Session],
    /// This request's session.
    pub current: Option<[u8; 32]>,
    pub passkeys: &'a [Passkey],
    /// Books with a reading position kept, and series with reader settings.
    pub positions: i64,
    pub series_settings: i64,
    /// Libraries and series in the account's own folder.
    pub own: (usize, i64),
    pub note: Option<&'a str>,
    pub error: Option<&'a str>,
}

/// `/account`.
pub fn account(me: &Me<'_>, libraries: &[Library], v: &AccountView<'_>) -> String {
    let now = crate::accounts::now();
    let mut m = String::new();
    settings_tabs(&mut m, "account", me);
    if let Some(n) = v.note {
        let _ = write!(m, "<p class=note role=status>{}</p>", escape(n));
    }
    m.push_str(&error_note(v.error));
    let a = v.account;
    let _ = write!(
        m,
        "<div class=pair><section class=box><h2>you</h2><div class=status><p><b>{}</b> {}</p>\
         <p>Your folder: <code>/library/~{}</code>, {} {}, {} series. Only you see it; nobody else, \
         the admin included.</p><p>Reading positions: {} books. Reader settings: {} series. They follow \
         you to every device you sign in on.</p></div></section>",
        escape(&a.name),
        role_tag(a.role),
        escape(&a.name),
        v.own.0,
        if v.own.0 == 1 { "library" } else { "libraries" },
        v.own.1,
        v.positions,
        v.series_settings
    );
    if a.password.is_some() {
        let _ = write!(
            m,
            "<form class=\"box form\" method=post action=/account/password><h2>password</h2>\
             <input type=hidden name=name autocomplete=username value=\"{}\">\
             <label>current password<input name=current type=password autocomplete=current-password required></label>\
             <label>new password<input name=password type=password autocomplete=new-password minlength=10 required></label>\
             <label>new password again<input name=again type=password autocomplete=new-password required></label>\
             <div class=row><button class=btn type=submit>change password</button>\
             <small>Signs out your other devices.</small></div></form></div>",
            escape(&a.name)
        );
    } else {
        m.push_str(
            "<section class=box><h2>password</h2><p class=hint>You sign in through the proxy, \
             or with a passkey; this account has no password.</p></section></div>",
        );
    }
    m.push_str("<section class=box><h2>passkeys</h2>");
    if v.passkeys.is_empty() {
        m.push_str("<p class=hint>None yet.</p>");
    } else {
        m.push_str(
            "<table class=tbl><thead><tr><th scope=col>name<th scope=col class=opt>added\
             <th scope=col>last used<th scope=col class=r><span class=hint>actions</span></thead><tbody>",
        );
        for p in v.passkeys {
            let id = crate::b64::encode(&p.id);
            let _ = write!(
                m,
                "<tr><td>{}<td class=opt>{}<td>{}<td><form method=post action=/account/passkey class=acts>\
                 <input type=hidden name=id value=\"{id}\"><input name=name aria-label=\"new name\" \
                 value=\"{}\" size=10><button type=submit name=do value=rename>rename</button>\
                 <button type=submit name=do value=remove>remove</button></form>",
                escape(&p.name),
                crate::html::date(p.added),
                p.used.map_or_else(|| "never".to_owned(), |t| ago(t, now)),
                escape(&p.name)
            );
        }
        m.push_str("</tbody></table>");
    }
    m.push_str(
        "<div class=\"row form\"><button class=btn type=button id=pk-add hidden>add a passkey</button>\
         <small>Uses this device's screen lock or a security key. Needs the site on HTTPS \
         (or localhost).</small></div><p id=pk-msg class=\"note err\" hidden></p></section>",
    );
    m.push_str(
        "<section class=box><h2>signed in</h2><table class=tbl><thead><tr><th scope=col>device\
         <th scope=col class=opt>from<th scope=col>last seen<th scope=col class=r>\
         <span class=hint>actions</span></thead><tbody>",
    );
    for s in v.sessions {
        let this = v.current == Some(s.hash);
        let _ = write!(
            m,
            "<tr><td>{}{}<td class=opt>{}<td>{}<td>",
            escape(&s.agent),
            if this {
                " <span class=pending>· this one</span>"
            } else {
                ""
            },
            escape(&s.addr),
            ago(s.seen, now)
        );
        if !this {
            let _ = write!(
                m,
                "<form method=post action=/account/signout class=acts><input type=hidden name=s value={}>\
                 <button type=submit>sign out</button></form>",
                &blake3::Hash::from(s.hash).to_hex()[..16]
            );
        }
    }
    m.push_str(
        "</tbody></table><form method=post action=/account/signout><input type=hidden name=s value=others>\
         <button class=btn type=submit>sign out everywhere else</button></form>\
         <small class=hint>A device not seen for 90 days is signed out by itself; without “keep me \
         signed in”, when the browser closes or after a day unused.</small></section>",
    );
    let mut page = shell("Account", libraries, &here(me), &m);
    // The passkey script, only here and on the sign-in page.
    page = page.replacen(
        "</head>",
        &format!(
            "<script defer src=\"{}\"></script></head>",
            account_js_url()
        ),
        1,
    );
    page
}

/// One row of `/users`.
pub struct UserRow {
    pub account: Account,
    pub passkeys: usize,
    pub link: bool,
    pub series: i64,
}

/// `/users` (the admin's).
pub fn users(
    me: &Me<'_>,
    libraries: &[Library],
    rows: &[UserRow],
    made: Option<(&str, &str)>,
    error: Option<&str>,
    proxy: &crate::auth::Config,
) -> String {
    let now = crate::accounts::now();
    let mut m = String::new();
    settings_tabs(&mut m, "users", me);
    if let Some((name, url)) = made {
        let _ = write!(
            m,
            "<p class=note role=status>A sign-in link for <b>{}</b>. Give it to them; it works once, \
             for 24 hours, and lets them choose a password:<br><code>{}</code></p>",
            escape(name),
            escape(url)
        );
    }
    m.push_str(&error_note(error));
    m.push_str(
        "<div class=scroll><table class=tbl><thead><tr><th scope=col>name<th scope=col>role\
         <th scope=col class=opt>own folder<th scope=col class=opt>signs in with\
         <th scope=col class=opt>last seen<th scope=col class=r><span class=hint>actions</span></thead><tbody>",
    );
    for r in rows {
        let a = &r.account;
        let name = escape(&a.name);
        let role = if a.role == Role::Admin {
            role_tag(a.role)
        } else {
            let mut sel = format!(
                "<form method=post action=/users/role class=row><input type=hidden name=name value=\"{name}\">\
                 <select name=role aria-label=\"role of {name}\">"
            );
            for role in [Role::Reader, Role::Curator] {
                let _ = write!(
                    sel,
                    "<option{}>{}</option>",
                    if role == a.role { " selected" } else { "" },
                    role.as_str()
                );
            }
            sel.push_str("</select><button type=submit class=hint>set</button></form>");
            sel
        };
        let mut with = Vec::new();
        if a.password.is_some() {
            with.push("password".to_owned());
        }
        if r.passkeys > 0 {
            with.push(format!(
                "{} passkey{}",
                r.passkeys,
                if r.passkeys == 1 { "" } else { "s" }
            ));
        }
        let with = if !with.is_empty() {
            with.join(", ")
        } else if r.link {
            "<span class=pending>link not used yet</span>".to_owned()
        } else if proxy.header.is_some() {
            format!(
                "proxy ({})",
                escape(proxy.header.as_deref().unwrap_or_default())
            )
        } else {
            "nothing yet".to_owned()
        };
        let _ = write!(
            m,
            "<tr><td>{name}<td>{role}<td class=opt>~{name} · {}<td class=opt>{with}<td class=opt>{}<td>",
            if r.series == 0 {
                "none yet".to_owned()
            } else {
                format!("{} series", r.series)
            },
            a.seen.map_or_else(|| "never".to_owned(), |t| ago(t, now))
        );
        if a.role != Role::Admin {
            let _ = write!(
                m,
                "<form method=post class=acts><input type=hidden name=name value=\"{name}\">\
                 <button type=submit formaction=/users/link>new sign-in link</button>\
                 <button type=submit formaction=/users/remove>remove</button></form>"
            );
        }
    }
    m.push_str("</tbody></table></div><div class=pair>");
    m.push_str(
        "<form class=\"box form\" method=post action=/users/add><h2>add an account</h2>\
         <label>name<input name=name required autocapitalize=none pattern=\"[a-z0-9][a-z0-9._\\-]{0,31}\">\
         <small>Lower-case letters, digits, . _ -; also the name of its folder, \
         <code>/library/~name</code>.</small></label>\
         <label>role<select name=role><option>reader</option><option>curator</option></select></label>\
         <button class=\"btn primary\" type=submit>add and make a sign-in link</button></form>",
    );
    let others: Vec<&UserRow> = rows
        .iter()
        .filter(|r| r.account.role != Role::Admin)
        .collect();
    if !others.is_empty() {
        m.push_str(
            "<form class=\"box form\" method=post action=/users/handover><h2>hand over the admin role</h2>\
             <p class=hint>There is one admin. The account you choose becomes it, and you become a curator.</p>\
             <label>new admin<select name=to>",
        );
        for r in others {
            let _ = write!(m, "<option>{}</option>", escape(&r.account.name));
        }
        m.push_str(
            "</select></label><label>your password<input name=password type=password \
             autocomplete=current-password required></label>\
             <button class=\"btn danger\" type=submit>hand over</button></form>",
        );
    }
    m.push_str("</div><section class=box><h2>roles</h2><div class=status>\
        <p><span class=role>reader</span> reads and downloads, keeps its own progress and reader settings.</p>\
        <p><span class=role>curator</span> also organises the library: moves, trash, details, renames, \
        uploads, conversions, as they arrive.</p>\
        <p><span class=\"role admin\">admin</span> also adds and removes accounts, sets roles and edits \
        library settings.</p>\
        <p>Everyone has a private folder, <code>/library/~name</code>: the libraries in it are seen by \
        that account alone. Removing an account leaves its folder on disk, unseen, until an account of \
        that name exists again.</p>");
    match (&proxy.header, proxy.proxies.is_empty()) {
        (Some(h), false) => {
            let _ = write!(
                m,
                "<p>Sign-in through a proxy: <b>on</b>, header <code>{}</code>, believed only from the \
                 proxy's addresses (<code>--trust-header</code>, <code>--trust-proxy</code>). A name \
                 the proxy sends that has no account gets one, as a reader.</p>",
                escape(h)
            );
        }
        _ => m.push_str(
            "<p>Sign-in through a proxy: off (<code>--trust-header</code> and \
             <code>--trust-proxy</code> turn it on).</p>",
        ),
    }
    m.push_str("</div></section>");
    shell("Users", libraries, &here(me), &m)
}

/// `/settings`: the libraries the account sees, editable where it may
/// (`editable`): the admin the shared ones, each account its own.
pub fn settings(
    me: &Me<'_>,
    libraries: &[Library],
    editable: &dyn Fn(&Library) -> bool,
    queued: &[String],
    error: Option<&str>,
) -> String {
    let mut m = String::new();
    settings_tabs(&mut m, "libraries", me);
    for folder in queued {
        let name = libraries
            .iter()
            .find(|l| &l.folder == folder)
            .map_or(folder.as_str(), |l| l.name.as_str());
        let _ = write!(
            m,
            "<p class=note role=status><b>{}</b>: changed. <span class=pending>Queued</span>; the \
             worker applies it within a minute, then every page shows it.</p>",
            escape(name)
        );
    }
    m.push_str(&error_note(error));
    let (shared, own): (Vec<&Library>, Vec<&Library>) =
        libraries.iter().partition(|l| l.owner().is_none());
    for (i, list) in [shared, own].into_iter().enumerate() {
        if list.is_empty() {
            continue;
        }
        if i == 1 {
            let _ = write!(
                m,
                "<h2 class=section-h>only yours · /library/~{}</h2>",
                escape(me.name)
            );
        }
        m.push_str(
            "<div class=scroll><table class=tbl><thead><tr><th scope=col>library<th scope=col class=opt>folder\
             <th scope=col>type<th scope=col class=opt>direction<th scope=col class=opt>colour\
             <th scope=col class=opt>page copies\
             <th scope=col class=r>series<th scope=col class=r><span class=hint>actions</span></thead><tbody>",
        );
        for l in list {
            library_row(&mut m, l, editable(l), queued.contains(&l.folder));
        }
        m.push_str("</tbody></table></div>");
    }
    m.push_str(
        "<h2 class=section-h>where these come from</h2><div class=status>\
         <p>Every top-level folder in /library that holds series folders is a library; series, books \
         and photo folders straight in /library are in “Library”. A folder named <code>~name</code> \
         is that account's own: the libraries in it are seen by that account alone, which also edits \
         their settings.</p><p>Until changed here, a library's settings come from its folder: name \
         capitalised; type comics if it says “comic”, cosplay if it says “cosplay”, else manga; \
         direction right to left for manga, left to right otherwise; colour the next in a fixed list. \
         A book's own metadata still wins over the direction.</p><p>Page copies: the widths of the \
         smaller copies the worker keeps of every large page, for the reader. Fewer take less disk; \
         the reader then loads the next larger copy, or the page itself. A change is made in the \
         background: copies no longer wanted are removed, new ones made from the pages.</p><p>Changes are kept in the catalogue, \
         not in /library: no file or folder is renamed. <code>koma serve</code> queues them and the \
         worker writes them, so the worker stays the catalogue's only writer.</p></div>",
    );
    shell("Settings", libraries, &here(me), &m)
}

/// A library's variant widths as the settings show them.
fn widths_label(widths: u8) -> &'static str {
    WIDTH_CHOICES
        .iter()
        .find(|c| c.0 == widths)
        .map_or("custom", |c| c.1)
}

fn library_row(m: &mut String, l: &Library, editable: bool, queued: bool) {
    let folder = if l.folder.is_empty() {
        "/library".to_owned()
    } else {
        format!("/library/{}", l.folder)
    };
    if !editable {
        let _ = write!(
            m,
            "<tr><td><a href=\"{}\">{}</a><td class=opt>{}<td>{}<td class=opt>{}\
             <td class=opt><span class=swatch style=\"--c:{}\"></span> {}<td class=opt>{}<td class=r>{}<td>",
            l.url(),
            escape(&l.name),
            escape(&folder),
            l.kind.as_str(),
            l.direction.as_str(),
            l.colour.1,
            l.colour.0,
            widths_label(l.widths),
            l.series
        );
        return;
    }
    let form = format!(
        "f-{}",
        blake3::hash(l.folder.as_bytes())
            .to_hex()
            .get(..8)
            .unwrap_or_default()
    );
    let label = escape(&l.name);
    let _ = write!(
        m,
        "<tr><td><input form={form} name=name value=\"{label}\" size=12 maxlength=64 required \
         aria-label=\"name of {label}\"><td class=opt>{}<td><select form={form} name=kind aria-label=\"type of {label}\">",
        escape(&folder)
    );
    for k in Kind::ALL {
        let _ = write!(
            m,
            "<option{}>{}</option>",
            if k == l.kind { " selected" } else { "" },
            k.as_str()
        );
    }
    let _ = write!(
        m,
        "</select><td class=opt><select form={form} name=dir aria-label=\"direction of {label}\">"
    );
    for d in [Direction::RightToLeft, Direction::LeftToRight] {
        let _ = write!(
            m,
            "<option value={}{}>{}</option>",
            d.code(),
            if d == l.direction { " selected" } else { "" },
            d.as_str()
        );
    }
    let _ = write!(
        m,
        "</select><td class=opt><span class=swatch style=\"--c:{}\"></span> \
         <select form={form} name=colour aria-label=\"colour of {label}\">",
        l.colour.1
    );
    for (name, _) in COLOURS {
        let _ = write!(
            m,
            "<option{}>{name}</option>",
            if name == l.colour.0 { " selected" } else { "" }
        );
    }
    let _ = write!(
        m,
        "</select><td class=opt><select form={form} name=widths aria-label=\"page copies of {label}\">"
    );
    for (bits, text) in WIDTH_CHOICES {
        let _ = write!(
            m,
            "<option value={bits}{}>{text}</option>",
            if bits == l.widths { " selected" } else { "" }
        );
    }
    let _ = write!(
        m,
        "</select><td class=r>{}<td><form id={form} method=post action=/settings class=acts>\
         <input type=hidden name=folder value=\"{}\">{}</form>",
        l.series,
        escape(&l.folder),
        if queued {
            "<span class=pending>queued</span>"
        } else {
            "<button type=submit>save</button>"
        }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_ago() {
        assert_eq!(ago(1000, 1010), "now");
        assert_eq!(ago(1000, 1000 + 300), "5 min ago");
        assert_eq!(ago(0, 3 * 3600), "3 hours ago");
        assert_eq!(ago(0, 90_000), "yesterday");
        assert_eq!(ago(0, 5 * 86_400), "5 days ago");
    }

    #[test]
    fn user_menu_escapes_and_shows_admin_links() {
        let html = user_menu(&Me {
            name: "mirelle",
            role: Role::Admin,
        });
        assert!(html.contains("href=/users"));
        let html = user_menu(&Me {
            name: "bram",
            role: Role::Reader,
        });
        assert!(!html.contains("href=/users"));
        assert!(html.contains("action=/signout"));
    }
}
