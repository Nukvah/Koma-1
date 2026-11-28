//! Server-rendered pages: libraries, series, settings and the bare reader.
//!
//! Plain HTML with no JavaScript. Library pages follow docs/design and share
//! one stylesheet, `assets/koma.css`, served at a content-addressed URL
//! ([`css_url`]) and cached for good. The reader keeps a few inline rules
//! until its own design (M4).

use std::fmt::Write;

use std::sync::LazyLock;

use crate::catalog::{Book, Cover, Page, Series, TocRow};
use crate::libraries::Library;
use crate::pages::Me;
use crate::variants::{self, Format};

/// Escapes text for HTML element content and quoted attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// The URL of a page's bytes: content-addressed, so it never changes.
pub fn page_url(page: &Page) -> String {
    format!("/p/{}.{}", page.hash.to_hex(), page.format)
}

/// The stylesheet of every page but the reader.
pub const CSS: &str = include_str!("../assets/koma.css");

static CSS_URL: LazyLock<String> = LazyLock::new(|| {
    let hex = blake3::hash(CSS.as_bytes()).to_hex();
    format!("/a/koma.{}.css", &hex[..16])
});

/// The stylesheet's URL: named by its content, so it is cached for good and
/// a new version gets a new URL.
pub fn css_url() -> &'static str {
    &CSS_URL
}

/// Infinite scroll for library grids: the only script outside the reader.
/// Pages work without it.
pub const SCROLL_JS: &str = include_str!("../assets/library/scroll.js");

static SCROLL_URL: LazyLock<String> = LazyLock::new(|| {
    let hex = blake3::hash(SCROLL_JS.as_bytes()).to_hex();
    format!("/a/scroll.{}.js", &hex[..16])
});

/// The scroll script's URL, named by its content like [`css_url`].
pub fn scroll_url() -> &'static str {
    &SCROLL_URL
}

/// Series and collections on one library page; more are on `?page=` pages.
pub const PER_PAGE: usize = 24;

/// Width of the page variant shown on cover cards.
pub const COVER_WIDTH: u32 = 720;

/// Background of a cover with no picture, by series id.
const SHADES: [&str; 8] = [
    "#1E3A6E", "#2F5E4C", "#6B3426", "#4E3A6B", "#6E5A22", "#1F5468", "#704622", "#3F4C5C",
];

fn document(title: &str, head_extra: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{}</title>{head_extra}</head><body>{body}</body></html>\n",
        escape(title)
    )
}

/// What the side panel shows as open: a library, and maybe one collection.
pub struct Here<'a> {
    pub library: Option<&'a Library>,
    pub collections: &'a [String],
    pub collection: Option<&'a str>,
    /// Which `// go` link is current: "library" or "settings".
    pub go: &'static str,
    /// The search box's text.
    pub query: &'a str,
    /// Loads the infinite-scroll script (library grids).
    pub scroll: bool,
    /// Who is signed in: the user menu.
    pub me: &'a Me<'a>,
}

/// A page in the shell: top bar, side panel (folded behind `menu` on
/// phones, without JavaScript) and `main`.
pub(crate) fn shell(title: &str, libraries: &[Library], here: &Here<'_>, main: &str) -> String {
    let mut b = String::with_capacity(main.len() + 2048);
    let _ = write!(
        b,
        "<input type=checkbox id=menu class=menu aria-label=\"show the menu\">\
         <header class=top><a class=brand href=\"/\">KOMA-1</a>\
         <form class=search role=search action=\"/search\"><label for=q>/</label>\
         <input id=q name=q type=search value=\"{}\" \
         placeholder=\"search all libraries: title, author:, tag:\"></form>\
         <label for=menu class=menu-btn aria-hidden=true>menu</label>{}</header>\
         <div class=layout><aside class=side aria-label=\"Navigation and libraries\">\
         <nav aria-label=Main><h2>// go</h2><ul>",
        escape(here.query),
        crate::pages::user_menu(here.me)
    );
    for (name, href) in [
        ("library", "/"),
        ("add files", "/add"),
        ("jobs", "/jobs"),
        ("trash", "/trash"),
        ("settings", "/settings"),
    ] {
        let on = here.go == name;
        let _ = write!(
            b,
            "<li><a href=\"{href}\"{}>{} {name}</a></li>",
            if on { " aria-current=page" } else { "" },
            if on { "&gt;" } else { "&nbsp;" }
        );
    }
    b.push_str("</ul></nav><hr><section class=tree><h2>// libraries</h2><ul>");
    let (shared, own): (Vec<&Library>, Vec<&Library>) =
        libraries.iter().partition(|l| l.owner().is_none());
    for (i, list) in [shared, own].into_iter().enumerate() {
        if i == 1 {
            if list.is_empty() {
                break;
            }
            b.push_str("</ul></section><hr><section class=tree><h2>// only yours</h2><ul>");
        }
        for l in list {
            tree_entry(&mut b, l, here);
        }
    }
    b.push_str("</ul></section></aside><main>");
    b.push_str(main);
    b.push_str("</main></div>");
    document(
        &format!("{title} · Koma-1"),
        &format!(
            "<link rel=stylesheet href=\"{}\">{}",
            css_url(),
            if here.scroll {
                format!("<script defer src=\"{}\"></script>", scroll_url())
            } else {
                String::new()
            }
        ),
        &b,
    )
}

/// A library in the side panel; the open one with its collections.
fn tree_entry(b: &mut String, l: &Library, here: &Here<'_>) {
    let open = here.library.is_some_and(|h| h.folder == l.folder);
    let _ = write!(
        b,
        "<li><a{} href=\"{}\">{} {}<span class=n>{}</span></a></li>",
        if open {
            " class=on aria-current=true"
        } else {
            ""
        },
        l.url(),
        if open { "▾" } else { "▸" },
        escape(&l.name),
        l.series
    );
    if open {
        let last = here.collections.len().saturating_sub(1);
        for (i, c) in here.collections.iter().enumerate() {
            let _ = write!(
                b,
                "<li><a class=sub href=\"{}\"{}>  {} {}</a></li>",
                collection_url(l, c),
                if here.collection == Some(c.as_str()) {
                    " aria-current=page"
                } else {
                    ""
                },
                if i == last { "└─" } else { "├─" },
                escape(c)
            );
        }
    }
}

fn collection_url(library: &Library, collection: &str) -> String {
    format!(
        "{}?c={}",
        library.url(),
        crate::libraries::encode_path_part(collection)
    )
}

/// A cover: the page's variant at [`COVER_WIDTH`] (WebP where the browser
/// takes it, else JPEG), else the page itself; over a coloured panel with
/// `label`, seen while it loads or if there is no page.
pub(crate) fn cover(out: &mut String, cover: Option<&Cover>, shade: &str, label: &str) {
    cover_with(out, cover, shade, label, "");
}

/// [`cover`] with `extra` inside it, over the picture: a corner tag, the
/// edge strip of progress.
pub(crate) fn cover_with(
    out: &mut String,
    cover: Option<&Cover>,
    shade: &str,
    label: &str,
    extra: &str,
) {
    // The label first, so the picture (positioned over the whole cover)
    // hides it once it has loaded.
    let _ = write!(
        out,
        "<div class=cover style=\"--c:{shade}\"><span class=t>{}</span>",
        escape(label)
    );
    if let Some(c) = cover {
        let has = |f: Format| c.variant_formats.iter().any(|x| x == f.ext());
        let src = match (has(Format::Jpeg), has(Format::Webp)) {
            (true, _) => variants::url(&c.hash, COVER_WIDTH, Format::Jpeg),
            (false, true) => variants::url(&c.hash, COVER_WIDTH, Format::Webp),
            (false, false) => format!("/p/{}.{}", c.hash.to_hex(), c.format),
        };
        out.push_str("<picture>");
        if has(Format::Webp) && has(Format::Jpeg) {
            let _ = write!(
                out,
                "<source type=image/webp srcset=\"{}\">",
                variants::url(&c.hash, COVER_WIDTH, Format::Webp)
            );
        }
        let _ = write!(out, "<img src=\"{src}\" alt=\"\" loading=lazy></picture>");
    }
    out.push_str(extra);
    out.push_str("</div>");
}

/// `1.4 GB`, `812 MB`, `96 KB`: decimal units, as file managers show them.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut size = bytes as f64 / 1000.0;
    for unit in UNITS {
        if size < 999.5 || unit == "TB" {
            return if size < 10.0 {
                format!("{size:.1} {unit}")
            } else {
                format!("{size:.0} {unit}")
            };
        }
        size /= 1000.0;
    }
    unreachable!()
}

pub(crate) fn shade(id: i64) -> &'static str {
    SHADES[id.rem_euclid(SHADES.len() as i64) as usize]
}

pub(crate) fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Where a book is read from: an EPUB in the EPUB reader (which goes on
/// where it was left), any other book from its first page.
pub fn read_url(book: &Book) -> String {
    if crate::scan::is_epub(&book.path) {
        format!("/e/{}/1", book.id)
    } else {
        format!("/r/{}/1", book.id)
    }
}

/// A book's length: chapters for an EPUB, else pages.
fn length(book: &Book) -> String {
    if crate::scan::is_epub(&book.path) {
        plural(book.page_count, "chapter", "chapters")
    } else {
        plural(book.page_count, "page", "pages")
    }
}

/// One card on a library page.
pub enum Item {
    Series {
        series: Series,
        books: i64,
        cover: Option<Cover>,
    },
    Collection {
        name: String,
        series: i64,
        books: i64,
        cover: Option<Cover>,
    },
}

/// A library page: collections first (as stacks), then series, a page of
/// [`PER_PAGE`] at a time. `page` is 1-based; `total` counts every card.
pub struct Shelf<'a> {
    pub libraries: &'a [Library],
    pub library: &'a Library,
    pub collections: &'a [String],
    /// When showing one collection's series.
    pub collection: Option<&'a str>,
    pub items: &'a [Item],
    pub page: usize,
    pub total: usize,
    pub me: &'a Me<'a>,
}

pub fn library(shelf: &Shelf<'_>) -> String {
    let l = shelf.library;
    let mut m = String::new();
    shelf_toolbar(&mut m, l, shelf.collection, shelf.total, false);
    m.push_str("<div class=grid data-scroll>");
    for item in shelf.items {
        match item {
            Item::Series {
                series,
                books,
                cover: c,
            } => {
                let _ = write!(m, "<a class=card href=\"/s/{}\">", series.id);
                cover(&mut m, c.as_ref(), shade(series.id), &series.name);
                let _ = write!(
                    m,
                    "<b>{}</b><span>{}</span></a>",
                    escape(&series.name),
                    plural(*books, "book", "books")
                );
            }
            Item::Collection {
                name,
                series,
                books,
                cover: c,
            } => {
                let _ = write!(
                    m,
                    "<a class=card href=\"{}\"><div class=stack>",
                    collection_url(l, name)
                );
                cover(&mut m, c.as_ref(), shade(name.len() as i64), name);
                let _ = write!(
                    m,
                    "</div><b>{}</b><span>{} · {}</span></a>",
                    escape(name),
                    plural(*series, "series", "series"),
                    plural(*books, "book", "books")
                );
            }
        }
    }
    m.push_str("</div>");
    pager(&mut m, shelf.page, shelf.total, PER_PAGE, |n| {
        shelf_url(l, shelf.collection, &format!("page={n}"))
    });
    let title = match shelf.collection {
        Some(c) => format!("{c} · {}", l.name),
        None => l.name.clone(),
    };
    let here = Here {
        library: Some(l),
        collections: shelf.collections,
        collection: shelf.collection,
        go: "library",
        query: "",
        me: shelf.me,
        scroll: true,
    };
    shell(&title, shelf.libraries, &here, &m)
}

/// A library page's URL with the open collection kept: `extra` is more of
/// the query (`view=table`, `page=2`…), or empty.
fn shelf_url(library: &Library, collection: Option<&str>, extra: &str) -> String {
    let mut url = match collection {
        Some(c) => collection_url(library, c),
        None => library.url(),
    };
    if !extra.is_empty() {
        url.push(if collection.is_some() { '&' } else { '?' });
        url.push_str(extra);
    }
    url
}

/// Previous and next links for page `page` of `total` items, `per_page` a
/// page; `link(n)` is page `n`'s URL. Nothing if it all fits on one.
fn pager(
    m: &mut String,
    page: usize,
    total: usize,
    per_page: usize,
    link: impl Fn(usize) -> String,
) {
    let pages = total.div_ceil(per_page).max(1);
    if pages == 1 {
        return;
    }
    let first = (page - 1) * per_page + 1;
    let last = (page * per_page).min(total);
    m.push_str("<nav class=pager aria-label=Pages>");
    if page > 1 {
        let _ = write!(
            m,
            "<a href=\"{}\" rel=prev>‹ previous</a>",
            escape(&link(page - 1))
        );
    }
    let _ = write!(
        m,
        "<span class=next>{first}–{last} of {total} · page {page}/{pages}"
    );
    if page < pages {
        let _ = write!(
            m,
            " · <a href=\"{}\" rel=next>next ›</a>",
            escape(&link(page + 1))
        );
    }
    m.push_str("</span></nav>");
}

/// The title of a library page, with the grid/table switch.
fn shelf_toolbar(m: &mut String, l: &Library, collection: Option<&str>, total: usize, table: bool) {
    m.push_str("<div class=toolbar><h1>");
    let folder = if l.folder.is_empty() {
        "library"
    } else {
        &l.folder
    };
    match collection {
        None => {
            let _ = write!(m, "~/<b>{}</b>", escape(folder));
        }
        Some(c) => {
            let _ = write!(
                m,
                "~/<a href=\"{}\">{}</a>/<b>{}</b>",
                l.url(),
                escape(folder),
                escape(c)
            );
        }
    }
    let _ = write!(
        m,
        " {} · {} · {}</h1><div class=seg><a href=\"{}\"{}>grid</a><a href=\"{}\"{}>table</a></div></div>",
        plural(
            total as i64,
            if table { "series" } else { "item" },
            if table { "series" } else { "items" }
        ),
        l.kind.as_str(),
        l.direction.as_str(),
        escape(&shelf_url(l, collection, "")),
        if table { "" } else { " aria-current=page" },
        escape(&shelf_url(l, collection, "view=table")),
        if table { " aria-current=page" } else { "" },
    );
}

/// Rows a table page shows.
pub const TABLE_PER_PAGE: usize = 50;

/// What the library table is sorted by: a column, and whether reversed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortKey {
    Title,
    Author,
    Books,
    Pages,
    Added,
}

impl SortKey {
    pub const ALL: [SortKey; 5] = [
        SortKey::Title,
        SortKey::Author,
        SortKey::Books,
        SortKey::Pages,
        SortKey::Added,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            SortKey::Title => "title",
            SortKey::Author => "author",
            SortKey::Books => "books",
            SortKey::Pages => "pages",
            SortKey::Added => "added",
        }
    }

    /// `title`, `-pages`… as in `?sort=`.
    pub fn parse(s: &str) -> Option<(SortKey, bool)> {
        let (reversed, key) = match s.strip_prefix('-') {
            Some(k) => (true, k),
            None => (false, s),
        };
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == key)
            .map(|k| (k, reversed))
    }
}

/// A library (or collection) as a sortable table, a page of rows.
pub struct Table<'a> {
    pub libraries: &'a [Library],
    pub library: &'a Library,
    pub collections: &'a [String],
    pub collection: Option<&'a str>,
    pub rows: &'a [crate::catalog::SeriesRow],
    pub sort: (SortKey, bool),
    pub page: usize,
    pub total: usize,
    pub me: &'a Me<'a>,
}

/// `YYYY-MM-DD` (UTC) of unix seconds.
pub fn date(unix: i64) -> String {
    // Days to civil date: Howard Hinnant's algorithm.
    let z = unix.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn table(t: &Table<'_>) -> String {
    let l = t.library;
    let mut m = String::new();
    shelf_toolbar(&mut m, l, t.collection, t.total, true);
    let sort_param = |(key, reversed): (SortKey, bool)| {
        format!(
            "view=table&sort={}{}",
            if reversed { "-" } else { "" },
            key.as_str()
        )
    };
    // A collection column only for a whole library that has collections.
    let show_collection = t.collection.is_none() && !t.collections.is_empty();
    m.push_str("<table class=tbl><thead><tr>");
    let columns: [(SortKey, &str, &str); 5] = [
        (SortKey::Title, "title", ""),
        (SortKey::Author, "author", " class=opt"),
        (SortKey::Books, "books", " class=r"),
        (SortKey::Pages, "pages", " class=\"r opt\""),
        (SortKey::Added, "added", " class=opt"),
    ];
    for (i, (key, label, class)) in columns.iter().enumerate() {
        if i == 1 && show_collection {
            m.push_str("<th scope=col class=opt>collection");
        }
        if i == 4 {
            m.push_str("<th scope=col class=opt>genres");
        }
        let current = t.sort.0 == *key;
        // A click on the current column turns it round.
        let next = (*key, current && !t.sort.1);
        let _ = write!(
            m,
            "<th scope=col{class}><a href=\"{}\"{}>{label}{}</a>",
            escape(&shelf_url(l, t.collection, &sort_param(next))),
            match (current, t.sort.1) {
                (true, false) => " aria-sort=ascending",
                (true, true) => " aria-sort=descending",
                _ => "",
            },
            match (current, t.sort.1) {
                (true, false) => " ▴",
                (true, true) => " ▾",
                _ => "",
            },
        );
    }
    m.push_str("</thead><tbody>");
    for r in t.rows {
        let _ = write!(
            m,
            "<tr><td><a href=\"/s/{}\">{}</a>",
            r.series.id,
            escape(&r.series.name)
        );
        if show_collection {
            let c = crate::libraries::collection_of(&r.series.path).unwrap_or("");
            let _ = write!(m, "<td class=opt>{}", escape(c));
        }
        let _ = write!(
            m,
            "<td class=opt>{}<td class=r>{}<td class=\"r opt\">{}<td class=opt>{}<td class=opt>{}",
            escape(&r.authors),
            r.books,
            r.pages,
            escape(&r.genres),
            date(r.added)
        );
    }
    m.push_str("</tbody></table>");
    pager(&mut m, t.page, t.total, TABLE_PER_PAGE, |n| {
        shelf_url(l, t.collection, &format!("{}&page={n}", sort_param(t.sort)))
    });
    let here = Here {
        library: Some(l),
        collections: t.collections,
        collection: t.collection,
        go: "library",
        query: "",
        me: t.me,
        scroll: false,
    };
    let title = match t.collection {
        Some(c) => format!("{c} · {} · table", l.name),
        None => format!("{} · table", l.name),
    };
    shell(&title, t.libraries, &here, &m)
}

/// What a series page shows besides the series: the account's progress,
/// select mode, and what it may do.
#[derive(Default)]
pub struct SeriesView<'a> {
    /// Per book: the page (or chapter) reached, and whether it is read.
    pub progress: std::collections::HashMap<i64, (i64, bool)>,
    /// Select mode: the books are a form, with the actions in a bar.
    pub select: bool,
    /// The account may organise this series.
    pub organise: bool,
    /// A note at the top, HTML (a change just made, with its undo).
    pub toast: Option<&'a str>,
}

/// A series: its cover and facts, then its books.
pub fn series(
    me: &Me<'_>,
    libraries: &[Library],
    library: &Library,
    collections: &[String],
    series: &Series,
    books: &[(Book, Option<Cover>)],
    view: &SeriesView<'_>,
) -> String {
    let mut m = String::from("<nav class=status aria-label=Breadcrumb>~/");
    let folder = if library.folder.is_empty() {
        "library"
    } else {
        &library.folder
    };
    let _ = write!(m, "<a href=\"{}\">{}</a>", library.url(), escape(folder));
    let collection = crate::libraries::collection_of(&series.path);
    if let Some(c) = collection {
        let _ = write!(
            m,
            "/<a href=\"{}\">{}</a>",
            collection_url(library, c),
            escape(c)
        );
    }
    let _ = write!(
        m,
        "/{}</nav>{}<div class=series><div class=\"art dots\">",
        escape(&series.name),
        view.toast.unwrap_or_default()
    );
    cover(
        &mut m,
        books.first().and_then(|(_, c)| c.as_ref()),
        shade(series.id),
        &series.name,
    );
    let (epubs, others): (Vec<&Book>, Vec<&Book>) = books
        .iter()
        .map(|(b, _)| b)
        .partition(|b| crate::scan::is_epub(&b.path));
    let sum = |v: &[&Book]| v.iter().map(|b| b.page_count).sum::<i64>();
    let mut length_chips = String::new();
    if !others.is_empty() {
        let _ = write!(
            length_chips,
            "<span>{}</span>",
            plural(sum(&others), "page", "pages")
        );
    }
    if !epubs.is_empty() {
        let _ = write!(
            length_chips,
            "<span>{}</span>",
            plural(sum(&epubs), "chapter", "chapters")
        );
    }
    let _ = write!(
        m,
        "</div><div class=meta><h1>{}</h1><div class=chips><span>{}</span>{length_chips}\
         <span>{}</span><span class=solid>{}</span></div><div class=actions>",
        escape(&series.name),
        plural(books.len() as i64, "book", "books"),
        library.kind.as_str(),
        library.direction.as_str()
    );
    // Where to go on: the first book started and not finished, else the
    // first not read, else the start.
    let next = books
        .iter()
        .find(|(b, _)| view.progress.get(&b.id).is_some_and(|&(_, done)| !done))
        .or_else(|| {
            books
                .iter()
                .find(|(b, _)| !view.progress.contains_key(&b.id))
        })
        .filter(|(b, _)| view.progress.contains_key(&b.id) || !view.progress.is_empty());
    match (next, books.first()) {
        (Some((b, _)), _) => {
            let at = view.progress.get(&b.id).map_or(1, |p| p.0.max(1));
            let _ = write!(
                m,
                "<a class=\"btn primary\" href=\"/r/{}/{at}\">read on: {}</a>",
                b.id,
                escape(&b.name)
            );
        }
        (None, Some((first, _))) => {
            let _ = write!(
                m,
                "<a class=\"btn primary\" href=\"{}\">read from the start</a>",
                read_url(first)
            );
        }
        _ => {}
    }
    if !view.select {
        let _ = write!(
            m,
            "<a class=btn href=\"/s/{}?select\">select</a>",
            series.id
        );
    }
    if view.organise && !view.select {
        let _ = write!(
            m,
            "<a class=btn href=\"/move?s={id}\">move or rename</a><a class=btn href=\"/naming?s={id}\">naming</a><a class=btn href=\"/add?s={id}\">add files</a>\
             <form method=post action=/organise><input type=hidden name=s value={id}>\
             <button class=\"btn danger\" type=submit name=action value=trash-series>delete series</button></form>",
            id = series.id
        );
    }
    let read = books
        .iter()
        .filter(|(b, _)| view.progress.get(&b.id).is_some_and(|p| p.1))
        .count();
    let _ = write!(
        m,
        "</div></div></div>{}<div class=toolbar><h2 class=section-h>books · {}{}</h2>{}</div>\
         <div class=\"grid vols\">",
        if view.select {
            format!(
                "<form method=post action=/organise><input type=hidden name=s value={}>",
                series.id
            )
        } else {
            String::new()
        },
        books.len(),
        if read > 0 {
            format!(" · {read} read")
        } else {
            String::new()
        },
        if view.select {
            format!("<a href=\"/s/{}\">done</a>", series.id)
        } else {
            String::new()
        }
    );
    for (book, c) in books {
        let progress = view.progress.get(&book.id).copied();
        let (class, extra) = match progress {
            Some((_, true)) => (
                " done",
                "<span class=tag>READ</span><span class=bar><i style=\"--p:100%\"></i></span>"
                    .to_owned(),
            ),
            Some((at, false)) => {
                let pct = (at * 100 / book.page_count.max(1)).clamp(1, 99);
                (
                    "",
                    format!(
                        "<span class=tag>{pct}%</span><span class=bar><i style=\"--p:{pct}%\"></i></span>"
                    ),
                )
            }
            None => ("", String::new()),
        };
        if view.select {
            let _ = write!(
                m,
                "<label class=\"pick{class}\"><input type=checkbox name=b value={} aria-label=\"{}\">",
                book.id,
                escape(&book.name)
            );
            cover_with(&mut m, c.as_ref(), shade(series.id), &book.name, &extra);
            let _ = write!(
                m,
                "<b>{}</b><span>{}</span></label>",
                escape(&book.name),
                length(book)
            );
            continue;
        }
        let href = match progress {
            Some((at, false)) if at > 1 && !crate::scan::is_epub(&book.path) => {
                format!("/r/{}/{at}", book.id)
            }
            _ => read_url(book),
        };
        let _ = write!(
            m,
            "<div class=\"vol{class}\"><a class=card href=\"{href}\">"
        );
        cover_with(&mut m, c.as_ref(), shade(series.id), &book.name, &extra);
        let _ = write!(
            m,
            "<b>{}</b><span>{}</span></a><a class=dl href=\"/d/{}\" download>download · {}{}</a></div>",
            escape(&book.name),
            length(book),
            book.id,
            human_size(book.size),
            if book.photos { " zip" } else { "" }
        );
    }
    m.push_str("</div>");
    if view.select {
        m.push_str(
            "<div class=actbar><b>select books, then</b>\
             <button class=btn type=submit name=action value=read>mark read</button>\
             <button class=btn type=submit name=action value=unread>mark unread</button>",
        );
        if view.organise {
            m.push_str(
                "<button class=\"btn primary\" type=submit name=action value=move>move to…</button>\
                 <button class=btn type=submit name=action value=details>edit details</button>\
                 <button class=\"btn danger\" type=submit name=action value=trash>delete</button>",
            );
        }
        m.push_str("</div></form>");
    }
    let here = Here {
        library: Some(library),
        collections,
        collection,
        go: "library",
        query: "",
        me,
        scroll: false,
    };
    shell(&series.name, libraries, &here, &m)
}

/// Nothing scanned yet.
pub fn empty(me: &Me<'_>) -> String {
    let main = "<div class=empty><span class=label>no books</span>\
        <strong>Nothing here yet</strong><p>The worker adds books as it scans /library. \
        Each top-level folder that holds series folders becomes a library: \
        /library/manga, /library/comics…</p></div>";
    let here = Here {
        library: None,
        collections: &[],
        collection: None,
        go: "library",
        query: "",
        me,
        scroll: false,
    };
    shell("Library", &[], &here, main)
}

/// `text`, escaped, with every range of `marks` (byte offsets from
/// [`crate::search::matches`]) in a `<mark>`.
fn marked(text: &str, marks: &[(usize, usize)]) -> String {
    let mut out = String::new();
    let mut at = 0;
    for &(from, to) in marks {
        out.push_str(&escape(&text[at..from]));
        let _ = write!(out, "<mark>{}</mark>", escape(&text[from..to]));
        at = to;
    }
    out.push_str(&escape(&text[at..]));
    out
}

/// What a search found, for [`search`].
pub struct Found<'a> {
    pub libraries: &'a [Library],
    pub query: &'a str,
    /// Words to mark in results.
    pub needles: &'a [&'a str],
    /// The library searched, or all.
    pub library: Option<&'a Library>,
    /// `(series, books, cover, library name)`.
    pub series: &'a [(Series, i64, Option<Cover>, String)],
    /// `(book, its series' name, library name)`.
    pub books: &'a [(Book, String, String)],
    /// `(kind, name, books)`.
    pub terms: &'a [(String, String, i64)],
    pub me: &'a Me<'a>,
}

pub fn search(f: &Found<'_>) -> String {
    let q = f.query.trim();
    let mut m = String::from("<div class=toolbar><h1>search ");
    let _ = write!(
        m,
        "<b>{}</b> · {} · {}",
        escape(q),
        plural(f.series.len() as i64, "series", "series"),
        plural(f.books.len() as i64, "book", "books")
    );
    let encoded = crate::libraries::encode_path_part(q);
    let _ = write!(
        m,
        "</h1><div class=seg><a href=\"/search?q={encoded}\"{}>all</a>",
        if f.library.is_none() {
            " aria-current=page"
        } else {
            ""
        }
    );
    for l in f.libraries {
        let on = f.library.is_some_and(|x| x.folder == l.folder);
        let _ = write!(
            m,
            "<a href=\"/search?q={encoded}&amp;l={}\"{}>{}</a>",
            crate::libraries::encode_path_part(&l.folder),
            if on { " aria-current=page" } else { "" },
            escape(&l.name.to_lowercase())
        );
    }
    m.push_str("</div></div>");
    let mark = |text: &str| marked(text, &crate::search::matches(text, f.needles));
    if q.is_empty() {
        m.push_str(
            "<div class=empty><strong>Search your libraries</strong><p>Type part of a \
             title, a person, a genre or a tag in the box above.</p></div>",
        );
    } else if f.series.is_empty() && f.books.is_empty() && f.terms.is_empty() {
        let _ = write!(
            m,
            "<div class=empty><strong>Nothing matches “{}”</strong><p>Search looks at \
             titles, people, genres and tags, in every library. Try fewer letters, or \
             <kbd>tag:</kbd> <kbd>author:</kbd> <kbd>genre:</kbd> to look in one place.</p></div>",
            escape(q)
        );
    }
    if !f.series.is_empty() {
        m.push_str("<h2 class=section-h>series</h2><div class=grid>");
        for (s, books, c, library) in f.series {
            let _ = write!(m, "<a class=card href=\"/s/{}\">", s.id);
            cover(&mut m, c.as_ref(), shade(s.id), &s.name);
            let _ = write!(
                m,
                "<b>{}</b><span>{} · {}</span></a>",
                mark(&s.name),
                plural(*books, "book", "books"),
                escape(library)
            );
        }
        m.push_str("</div>");
    }
    if !f.books.is_empty() {
        m.push_str("<h2 class=section-h>books</h2><div class=hits>");
        for (b, series, library) in f.books {
            let _ = write!(
                m,
                "<a href=\"{}\"><span>{} · <b>{}</b></span><small>{}</small></a>",
                read_url(b),
                escape(series),
                mark(&b.name),
                escape(library)
            );
        }
        m.push_str("</div>");
    }
    if !f.terms.is_empty() {
        m.push_str("<h2 class=section-h>people, genres and tags</h2><div class=chips>");
        for (kind, name, books) in f.terms {
            let field = match kind.as_str() {
                "genre" => "genre",
                "tag" => "tag",
                _ => "author",
            };
            let query = format!("{field}:\"{name}\"");
            let _ = write!(
                m,
                "<a href=\"/search?q={}\">{kind}: {} · {books}</a>",
                crate::libraries::encode_path_part(&query),
                mark(name)
            );
        }
        m.push_str("</div>");
    }
    m.push_str(
        "<p class=status>Matches any part of a word, so Japanese and Korean titles work \
         too. Narrow it with <kbd>author:</kbd> <kbd>tag:</kbd> <kbd>genre:</kbd>.</p>",
    );
    let here = Here {
        library: f.library,
        collections: &[],
        collection: None,
        go: "",
        query: q,
        me: f.me,
        scroll: false,
    };
    shell(&format!("Search: {q}"), f.libraries, &here, &m)
}

/// The reader's stylesheet and script, served like [`CSS`].
pub const READER_CSS: &str = include_str!("../assets/reader/reader.css");
pub const EPUB_CSS: &str = include_str!("../assets/epub/epub.css");
pub const EPUB_JS: &str = include_str!("../assets/epub/epub.js");

static EPUB_CSS_URL: LazyLock<String> = LazyLock::new(|| {
    let hex = blake3::hash(EPUB_CSS.as_bytes()).to_hex();
    format!("/a/epub.{}.css", &hex[..16])
});
static EPUB_JS_URL: LazyLock<String> = LazyLock::new(|| {
    let hex = blake3::hash(EPUB_JS.as_bytes()).to_hex();
    format!("/a/epub.{}.js", &hex[..16])
});

/// The EPUB reader's stylesheet's URL, named by its content.
pub fn epub_css_url() -> &'static str {
    &EPUB_CSS_URL
}

/// The EPUB reader's script's URL, named by its content.
pub fn epub_js_url() -> &'static str {
    &EPUB_JS_URL
}
pub const READER_JS: &str = include_str!("../assets/reader/reader.js");

static READER_CSS_URL: LazyLock<String> = LazyLock::new(|| {
    let hex = blake3::hash(READER_CSS.as_bytes()).to_hex();
    format!("/a/reader.{}.css", &hex[..16])
});
static READER_JS_URL: LazyLock<String> = LazyLock::new(|| {
    let hex = blake3::hash(READER_JS.as_bytes()).to_hex();
    format!("/a/reader.{}.js", &hex[..16])
});

pub fn reader_css_url() -> &'static str {
    &READER_CSS_URL
}

pub fn reader_js_url() -> &'static str {
    &READER_JS_URL
}

/// One page of a book for the reader: its size as displayed and the widths
/// of its variants, with which formats (bit 1: JPEG, bit 2: WebP).
pub struct ReaderPage {
    pub hash: blake3::Hash,
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub variants: Vec<(u32, u8)>,
}

impl ReaderPage {
    /// A page whose header gave no size is taken as a usual 2:3 page.
    pub fn new(page: &Page, variants: Vec<(u32, u8)>) -> Self {
        let (width, height) = page.size.map_or((1440, 2160), |s| (s.width, s.height));
        Self {
            hash: page.hash,
            format: page.format.clone(),
            width,
            height,
            variants,
        }
    }

    fn url(&self) -> String {
        format!("/p/{}.{}", self.hash.to_hex(), self.format)
    }

    /// `srcset` with the variants in `format` (bit), then the page itself.
    fn srcset(&self, bit: u8, format: Format) -> String {
        let mut set: Vec<String> = self
            .variants
            .iter()
            .filter(|(_, m)| m & bit != 0)
            .map(|(w, _)| format!("{} {w}w", variants::url(&self.hash, *w, format)))
            .collect();
        set.push(format!("{} {}w", self.url(), self.width));
        set.join(", ")
    }

    /// The width the page is shown at: the whole page in the window.
    fn sizes(&self) -> String {
        format!(
            "min(100vw, {:.1}vh)",
            100.0 * f64::from(self.width) / f64::from(self.height)
        )
    }
}

/// The book's pages for the reader's script: `{"v": variants version,
/// "p": [{"h", "f", "w", "ht", "s": [[width, formats]]}]}`.
/// `end`: the card shown at the end of `book`, as HTML: the book after it
/// in its series (`next`, with its cover), or back to the series.
pub fn reader_json(
    pages: &[ReaderPage],
    series: &Series,
    book: &Book,
    next: Option<&(Book, Option<Cover>)>,
    next_at: Option<i64>,
) -> String {
    let p: Vec<serde_json::Value> = pages
        .iter()
        .map(|p| {
            serde_json::json!({
                "h": p.hash.to_hex().to_string(),
                "f": p.format,
                "w": p.width,
                "ht": p.height,
                "s": p.variants,
            })
        })
        .collect();
    let back = escape(&series.name);
    let mut end = String::from("<div>");
    match next {
        Some((b, c)) => {
            cover(&mut end, c.as_ref(), shade(b.series_id), &b.name);
            let name = escape(&b.name);
            // Where the account left it, if it started it.
            let at = next_at.filter(|&n| n > 1 && n <= b.page_count);
            let (href, state) = match at {
                Some(n) => (format!("/r/{}/{n}", b.id), format!("at page {n}")),
                None => (read_url(b), "not started".to_owned()),
            };
            let _ = write!(
                end,
                "<section><p>end of {} · {} pages</p><strong>{name}</strong>\
                 <p>{} · <span class=at>{state}</span></p>\
                 <a class=\"btn primary\" href=\"{href}\" data-next={}>read {name} <kbd>space</kbd></a>\
                 <a class=btn href=\"/s/{}\">back to {back}</a></section>",
                escape(&book.name),
                book.page_count,
                length(b),
                b.id,
                series.id
            );
        }
        None => {
            let _ = write!(
                end,
                "<section><p>end of {} · {} pages</p><strong>{back}</strong><p>the last book</p>\
                 <a class=\"btn primary\" href=\"/s/{}\">back to {back}</a></section>",
                escape(&book.name),
                book.page_count,
                series.id
            );
        }
    }
    end.push_str("</div>");
    serde_json::json!({ "v": variants::VERSION, "p": p, "end": end }).to_string()
}

/// One page of a book, 1-based `number`: the page alone, tap thirds that
/// are links (in the book's direction), and bars behind `#bars`. With the
/// script, pages turn in place.
pub struct Reader<'a> {
    pub series: &'a Series,
    pub book: &'a Book,
    pub page: &'a ReaderPage,
    pub number: usize,
    pub total: usize,
    /// `true`: right to left.
    pub rtl: bool,
    /// What the account keeps for this book and series ([`crate::kept`]),
    /// a JSON object, for the script.
    pub kept: &'a str,
}

pub fn reader(r: &Reader<'_>) -> String {
    let (book, number, total) = (r.book, r.number, r.total);
    let up = format!("/s/{}", r.series.id);
    let at = |n: usize| format!("/r/{}/{n}", book.id);
    let prev = if number > 1 {
        at(number - 1)
    } else {
        up.clone()
    };
    let next = if number < total {
        at(number + 1)
    } else {
        up.clone()
    };
    let mut head = format!(
        "<link rel=stylesheet href=\"{}\"><script defer src=\"{}\"></script>",
        reader_css_url(),
        reader_js_url()
    );
    if number < total {
        let _ = write!(head, "<link rel=next href=\"{next}\">");
    }
    if number > 1 {
        let _ = write!(head, "<link rel=prev href=\"{prev}\">");
    }
    let p = r.page;
    let mut b = format!(
        "<main class=stage><picture><source id=pgw type=image/webp srcset=\"{}\" sizes=\"{}\">\
         <img id=pg src=\"{}\" srcset=\"{}\" sizes=\"{}\" width={} height={} alt=\"Page {number}\" \
         fetchpriority=high></picture></main>",
        p.srcset(2, Format::Webp),
        p.sizes(),
        p.url(),
        p.srcset(1, Format::Jpeg),
        p.sizes(),
        p.width,
        p.height
    );
    // In right-to-left books the left third goes forward.
    let (left, right) = if r.rtl {
        ((next.as_str(), "‹ next"), (prev.as_str(), "previous ›"))
    } else {
        ((prev.as_str(), "‹ previous"), (next.as_str(), "next ›"))
    };
    let _ = write!(
        b,
        "<nav class=zones aria-label=Pages><a href=\"{}\"><span>{}</span></a>\
         <a href=\"#bars\"><span>bars</span></a><a href=\"{}\"><span>{}</span></a></nav>",
        left.0, left.1, right.0, right.1
    );
    let _ = write!(
        b,
        "<div id=bars class=bars><header class=\"rbar top\"><a href=\"{up}\">‹ {}</a><b>{}</b>\
         <span class=\"end dim\"><span id=count>{number} / {total}</span><span class=keys> · <kbd>←</kbd><kbd>→</kbd> \
         <kbd>space</kbd> turn · <kbd>m</kbd> bars</span></span><a href=\"#\">close</a></header>\
         <footer class=\"rbar bot\"><div class=thumbs hidden></div>\
         <div class=scrub hidden><input id=scrub type=range min=1 max={total} value={number} \
         aria-label=Page><span class=peek></span></div>\
         <span class=\"dim label\">mode</span><div class=seg>\
         <a href=\"#bars\" data-mode=auto aria-current=true>auto</a><a href=\"#bars\" data-mode=single>single</a>\
         <a href=\"#bars\" data-mode=double>double</a><a href=\"#bars\" data-mode=strip>strip</a></div>\
         <a href=\"#bars\" data-shift>shift <kbd>s</kbd></a>\
         <span class=\"dim label\">fit</span><div class=seg>\
         <a href=\"#bars\" data-fit=page aria-current=true>page</a><a href=\"#bars\" data-fit=width>width</a>\
         <a href=\"#bars\" data-fit=height>height</a></div>\
         <span class=\"dim label\">direction</span><div class=seg>\
         <a href=\"#bars\" data-dir=rtl{}>← rtl</a><a href=\"#bars\" data-dir=ltr{}>ltr →</a></div>\
         <a class=end href=\"#bars\" data-zoom hidden>zoom <kbd>z</kbd></a>\
         <a href=\"#bars\" data-thumbs hidden>thumbnails <kbd>t</kbd></a></footer></div>",
        escape(&r.series.name),
        escape(&book.name),
        if r.rtl { " aria-current=true" } else { "" },
        if r.rtl { "" } else { " aria-current=true" },
    );
    let title = format!("{} · {number}/{total} · Koma-1", book.name);
    format!(
        "<!doctype html>\n<html lang=en data-kept=\"{}\"><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{}</title>{head}</head><body class=reader data-book={} data-series={} \
         data-n={number} data-dir={}>{b}</body></html>\n",
        escape(r.kept),
        escape(&title),
        book.id,
        r.series.id,
        if r.rtl { "rtl" } else { "ltr" }
    )
}

/// One chapter of an EPUB, 1-based `number`, as [`epub_chapter`] shows it.
pub struct EpubPage<'a> {
    pub series: &'a Series,
    pub book: &'a Book,
    pub number: usize,
    pub total: usize,
    /// The chapter, cleaned at ingest (see [`crate::epub::clean`]): written
    /// into the page as it is.
    pub html: &'a str,
    pub toc: &'a [TocRow],
    /// The book's language, `''` if unknown.
    pub language: &'a str,
    /// On the last chapter: the next book in the series, if there is one.
    pub next_book: Option<&'a Book>,
    /// What the account keeps for this book ([`crate::kept`]), JSON.
    pub kept: &'a str,
}

/// An EPUB chapter: a column of text on paper. Without JavaScript it reads,
/// with the series, contents (`#toc`) and the chapters around it as links;
/// the script adds size, width and spacing, the bars on a tap, and the
/// position kept in this browser.
pub fn epub_chapter(e: &EpubPage<'_>) -> String {
    let (book, n, total) = (e.book, e.number, e.total);
    let up = format!("/s/{}", e.series.id);
    let chapter_title = |i: usize| -> Option<&str> {
        e.toc
            .iter()
            .find(|t| t.chapter == i as i64 - 1)
            .map(|t| t.title.as_str())
    };
    let mut head = format!(
        "<link rel=stylesheet href=\"{}\"><link rel=stylesheet href=\"{}\"><script src=\"{}\"></script>",
        reader_css_url(),
        epub_css_url(),
        epub_js_url()
    );
    if n > 1 {
        let _ = write!(head, "<link rel=prev href=\"{}\">", n - 1);
    }
    if n < total {
        let _ = write!(
            head,
            "<link rel=next href=\"{}\"><link rel=prefetch href=\"{}\">",
            n + 1,
            n + 1
        );
    }
    let name = escape(&book.name);
    let series_name = escape(&e.series.name);
    let here = chapter_title(n).map(escape).unwrap_or_default();
    let mut b = String::new();
    // The bars, shown by the script on a tap.
    let _ = write!(
        b,
        "<header class=\"rbar top ebar\"><a href=\"{up}\">‹ {series_name}</a><b>{name}</b>\
         <span class=dim>chapter {n} of {total}</span><a href=\"#toc\">contents <kbd>c</kbd></a>\
         <span class=\"end dim label\" hidden>size</span><div class=seg hidden><a href=\"#\" data-size=-1>A−</a>\
         <span id=size>19</span><a href=\"#\" data-size=1>A+</a></div>\
         <span class=\"dim label\" hidden>width</span><div class=seg hidden><a href=\"#\" data-width=narrow>narrow</a>\
         <a href=\"#\" data-width=medium>medium</a><a href=\"#\" data-width=wide>wide</a></div>\
         <span class=\"dim label\" hidden>spacing</span><div class=seg hidden><a href=\"#\" data-space=tight>tight</a>\
         <a href=\"#\" data-space=normal>normal</a><a href=\"#\" data-space=loose>loose</a></div></header>"
    );
    // The contents.
    let _ = write!(
        b,
        "<nav id=toc class=toc aria-label=Contents><a class=shut href=\"#\">close</a>"
    );
    for t in e.toc {
        let at = usize::try_from(t.chapter + 1).unwrap_or(0);
        let anchor = t
            .anchor
            .as_deref()
            .map(|a| format!("#e-{}", escape(a)))
            .unwrap_or_default();
        let current = if at == n && t.anchor.is_none() {
            " aria-current=true"
        } else {
            ""
        };
        let _ = write!(
            b,
            "<a class=l{} href=\"{at}{anchor}\"{current}>{}</a>",
            t.level.clamp(1, 3),
            escape(&t.title)
        );
    }
    b.push_str("</nav>");
    // Where this is, for reading without the script.
    let _ = write!(
        b,
        "<nav class=head><a href=\"{up}\">‹ {series_name}</a><span>{name}</span>\
         <a href=\"#toc\">contents</a><span>chapter {n} of {total}</span></nav>"
    );
    let lang: String = e
        .language
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(35)
        .collect();
    if lang.is_empty() {
        b.push_str("<main>");
    } else {
        let _ = write!(b, "<main lang=\"{lang}\">");
    }
    b.push_str(e.html);
    b.push_str("</main><nav class=chap aria-label=Chapters>");
    if n > 1 {
        let t = chapter_title(n - 1)
            .map(escape)
            .unwrap_or_else(|| format!("chapter {}", n - 1));
        let _ = write!(b, "<a rel=prev href=\"{}\">‹ {t}</a>", n - 1);
    } else {
        let _ = write!(b, "<a href=\"{up}\">‹ {series_name}</a>");
    }
    if n < total {
        let t = chapter_title(n + 1)
            .map(escape)
            .unwrap_or_else(|| format!("chapter {}", n + 1));
        let _ = write!(b, "<a rel=next href=\"{}\">{t} ›</a>", n + 1);
    } else {
        // The end of the book: on to the next one, or back to the series.
        match e.next_book {
            Some(next) => {
                let _ = write!(
                    b,
                    "<span class=end>end of {name}</span><a rel=next href=\"{}\">read {} ›</a>",
                    read_url(next),
                    escape(&next.name)
                );
            }
            None => {
                let _ = write!(
                    b,
                    "<span class=end>end of {name}</span><a rel=next href=\"{up}\">back to {series_name} ›</a>"
                );
            }
        }
    }
    b.push_str("</nav>");
    let title = if here.is_empty() {
        format!("{name} · {n}/{total} · Koma-1")
    } else {
        format!("{here} · {name} · Koma-1")
    };
    format!(
        "<!doctype html>\n<html lang=en data-kept=\"{}\"><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title>{head}</head><body class=\"reader book\" data-book={} data-n={n} \
         data-total={total}>{b}</body></html>\n",
        escape(e.kept),
        book.id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(human_size(0), "0.0 KB");
        assert_eq!(human_size(96_400), "96 KB");
        assert_eq!(human_size(999_400), "999 KB");
        assert_eq!(human_size(999_600), "1.0 MB");
        assert_eq!(human_size(812_000_000), "812 MB");
        assert_eq!(human_size(1_410_000_000), "1.4 GB");
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_795_737_600), "2026-11-27");
        assert_eq!(date(-86_400), "1969-12-31");
    }

    #[test]
    fn sort_keys() {
        assert_eq!(SortKey::parse("pages"), Some((SortKey::Pages, false)));
        assert_eq!(SortKey::parse("-added"), Some((SortKey::Added, true)));
        assert_eq!(SortKey::parse("--added"), None);
        assert_eq!(SortKey::parse("colour"), None);
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(
            escape(r#"<a href="x">Mirelle's & co</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Mirelle&#39;s &amp; co&lt;/a&gt;"
        );
    }
}
