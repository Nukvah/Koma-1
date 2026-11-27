//! Server-rendered pages: libraries, series, settings and the bare reader.
//!
//! Plain HTML with no JavaScript. Library pages follow docs/design and share
//! one stylesheet, `assets/koma.css`, served at a content-addressed URL
//! ([`css_url`]) and cached for good. The reader keeps a few inline rules
//! until its own design (M4).

use std::fmt::Write;

use std::sync::LazyLock;

use crate::catalog::{Book, Cover, Page, Series};
use crate::libraries::Library;
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

const READER_STYLE: &str = "body{font-family:system-ui,sans-serif;margin:0;padding:0;background:#000;color:#ccc;text-align:center}\
a{color:inherit}img{display:block;margin:0 auto;max-width:100%;max-height:100vh}nav{padding:.5rem}";

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
}

/// A page in the shell: top bar, side panel (folded behind `menu` on
/// phones, without JavaScript) and `main`.
fn shell(title: &str, libraries: &[Library], here: &Here<'_>, main: &str) -> String {
    let mut b = String::with_capacity(main.len() + 2048);
    let _ = write!(
        b,
        "<input type=checkbox id=menu class=menu aria-label=\"show the menu\">\
         <header class=top><a class=brand href=\"/\">KOMA-1</a>\
         <form class=search role=search action=\"/search\"><label for=q>/</label>\
         <input id=q name=q type=search value=\"{}\" \
         placeholder=\"search all libraries: title, author:, tag:\"></form>\
         <label for=menu class=menu-btn aria-hidden=true>menu</label></header>\
         <div class=layout><aside class=side aria-label=\"Navigation and libraries\">\
         <nav aria-label=Main><h2>// go</h2><ul>",
        escape(here.query)
    );
    for (name, href) in [("library", "/"), ("settings", "/settings")] {
        let on = here.go == name;
        let _ = write!(
            b,
            "<li><a href=\"{href}\"{}>{} {name}</a></li>",
            if on { " aria-current=page" } else { "" },
            if on { "&gt;" } else { "&nbsp;" }
        );
    }
    b.push_str("</ul></nav><hr><section class=tree><h2>// libraries</h2><ul>");
    for l in libraries {
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
fn cover(out: &mut String, cover: Option<&Cover>, shade: &str, label: &str) {
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

fn shade(id: i64) -> &'static str {
    SHADES[id.rem_euclid(SHADES.len() as i64) as usize]
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
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
        scroll: false,
    };
    let title = match t.collection {
        Some(c) => format!("{c} · {} · table", l.name),
        None => format!("{} · table", l.name),
    };
    shell(&title, t.libraries, &here, &m)
}

/// A series: its cover and facts, then its books.
pub fn series(
    libraries: &[Library],
    library: &Library,
    collections: &[String],
    series: &Series,
    books: &[(Book, Option<Cover>)],
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
        "/{}</nav><div class=series><div class=\"art dots\">",
        escape(&series.name)
    );
    cover(
        &mut m,
        books.first().and_then(|(_, c)| c.as_ref()),
        shade(series.id),
        &series.name,
    );
    let pages: i64 = books.iter().map(|(b, _)| b.page_count).sum();
    let _ = write!(
        m,
        "</div><div class=meta><h1>{}</h1><div class=chips><span>{}</span><span>{}</span>\
         <span>{}</span><span class=solid>{}</span></div><div class=actions>",
        escape(&series.name),
        plural(books.len() as i64, "book", "books"),
        plural(pages, "page", "pages"),
        library.kind.as_str(),
        library.direction.as_str()
    );
    if let Some((first, _)) = books.first() {
        let _ = write!(
            m,
            "<a class=\"btn primary\" href=\"/r/{}/1\">read from the start</a>",
            first.id
        );
    }
    let _ = write!(
        m,
        "</div></div></div><div class=toolbar><h2 class=section-h>books · {}</h2></div>\
         <div class=\"grid vols\">",
        books.len()
    );
    for (book, c) in books {
        let _ = write!(m, "<div class=vol><a class=card href=\"/r/{}/1\">", book.id);
        cover(&mut m, c.as_ref(), shade(series.id), &book.name);
        let _ = write!(
            m,
            "<b>{}</b><span>{}</span></a><a class=dl href=\"/d/{}\" download>download · {}{}</a></div>",
            escape(&book.name),
            plural(book.page_count, "page", "pages"),
            book.id,
            human_size(book.size),
            if book.photos { " zip" } else { "" }
        );
    }
    m.push_str("</div>");
    let here = Here {
        library: Some(library),
        collections,
        collection,
        go: "library",
        query: "",
        scroll: false,
    };
    shell(&series.name, libraries, &here, &m)
}

/// Nothing scanned yet.
pub fn empty() -> String {
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
                "<a href=\"/r/{}/1\"><span>{} · <b>{}</b></span><small>{}</small></a>",
                b.id,
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
        scroll: false,
    };
    shell(&format!("Search: {q}"), f.libraries, &here, &m)
}

/// The libraries and their settings, read-only until accounts (M5).
pub fn settings(libraries: &[Library]) -> String {
    let mut m = String::from(
        "<div class=toolbar><h1>settings · <b>libraries</b></h1></div>\
         <p class=note>Read-only until accounts arrive: then an admin can change a \
         library's name, type, reading direction and colour here.</p>\
         <table class=tbl><thead><tr><th scope=col>library<th scope=col class=opt>folder\
         <th scope=col>type<th scope=col class=opt>direction<th scope=col class=opt>colour\
         <th scope=col class=r>series</thead><tbody>",
    );
    for l in libraries {
        let folder = if l.folder.is_empty() {
            "/library".to_owned()
        } else {
            format!("/library/{}", l.folder)
        };
        let _ = write!(
            m,
            "<tr><td><a href=\"{}\">{}</a><td class=opt>{}<td>{}<td class=opt>{}\
             <td class=opt><span class=swatch style=\"--c:{}\"></span> {}<td class=r>{}",
            l.url(),
            escape(&l.name),
            escape(&folder),
            l.kind.as_str(),
            l.direction.as_str(),
            l.colour.1,
            l.colour.0,
            l.series
        );
    }
    m.push_str(
        "</tbody></table><div class=status><p>Every top-level folder in /library that holds \
         series folders is a library; series, books and photo folders straight in /library \
         are in “Library”.</p><p>name: the folder's name, capitalised. type: comics if it \
         says “comic”, cosplay if it says “cosplay”, else manga. direction: right to left \
         for manga, left to right otherwise; a book's own metadata wins. colour: the next \
         in a fixed list.</p></div>",
    );
    let here = Here {
        library: None,
        collections: &[],
        collection: None,
        go: "settings",
        query: "",
        scroll: false,
    };
    shell("Settings", libraries, &here, &m)
}

/// One page of a book, 1-based `number`. The image links to the next page;
/// the last page links back to the series.
pub fn reader(series: &Series, book: &Book, pages: &[Page], number: usize) -> String {
    let total = pages.len();
    let page = &pages[number - 1];
    let up = format!("/s/{}", series.id);
    let prev = (number > 1).then(|| format!("/r/{}/{}", book.id, number - 1));
    let next = (number < total).then(|| format!("/r/{}/{}", book.id, number + 1));

    let mut head = String::new();
    if let Some(next) = &next {
        let _ = write!(head, "<link rel=next href=\"{next}\">");
        let next_image = page_url(&pages[number]);
        let _ = write!(head, "<link rel=prefetch href=\"{next_image}\">");
    }
    if let Some(prev) = &prev {
        let _ = write!(head, "<link rel=prev href=\"{prev}\">");
    }

    let link = |href: &Option<String>, text: &str, rel: &str| match href {
        Some(href) => format!("<a href=\"{href}\" rel={rel}>{text}</a>"),
        None => format!("<span>{text}</span>"),
    };
    let nav = format!(
        "<nav>{} · <a href=\"{up}\">{}</a> · {number} / {total} · {}</nav>",
        link(&prev, "‹ previous", "prev"),
        escape(&book.name),
        link(&next, "next ›", "next"),
    );
    let image_target = next.as_deref().unwrap_or(&up);
    let body = format!(
        "{nav}<a href=\"{image_target}\"><img src=\"{}\" alt=\"Page {number}\"></a>{nav}",
        page_url(page)
    );
    let title = format!("{} · {number}/{total} · Koma-1", book.name);
    document(
        &title,
        &format!("<style>{READER_STYLE}</style>{head}"),
        &body,
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
