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
}

/// A page in the shell: top bar, side panel (folded behind `menu` on
/// phones, without JavaScript) and `main`.
fn shell(title: &str, libraries: &[Library], here: &Here<'_>, main: &str) -> String {
    let mut b = String::with_capacity(main.len() + 2048);
    b.push_str(
        "<input type=checkbox id=menu class=menu aria-label=\"show the menu\">\
         <header class=top><a class=brand href=\"/\">KOMA-1</a><span class=search></span>\
         <label for=menu class=menu-btn aria-hidden=true>menu</label></header>\
         <div class=layout><aside class=side aria-label=\"Navigation and libraries\">\
         <nav aria-label=Main><h2>// go</h2><ul>",
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
        &format!("<link rel=stylesheet href=\"{}\">", css_url()),
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
    let mut m = String::from("<div class=toolbar><h1>");
    let folder = if l.folder.is_empty() {
        "library"
    } else {
        &l.folder
    };
    match shelf.collection {
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
        " {} · {} · {}</h1></div><div class=grid>",
        plural(shelf.total as i64, "item", "items"),
        l.kind.as_str(),
        l.direction.as_str()
    );
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
    pager(&mut m, shelf);
    let title = match shelf.collection {
        Some(c) => format!("{c} · {}", l.name),
        None => l.name.clone(),
    };
    let here = Here {
        library: Some(l),
        collections: shelf.collections,
        collection: shelf.collection,
        go: "library",
    };
    shell(&title, shelf.libraries, &here, &m)
}

fn pager(m: &mut String, shelf: &Shelf<'_>) {
    let pages = shelf.total.div_ceil(PER_PAGE).max(1);
    if pages == 1 {
        return;
    }
    let base = match shelf.collection {
        Some(c) => format!("{}&page=", collection_url(shelf.library, c)),
        None => format!("{}?page=", shelf.library.url()),
    };
    let first = (shelf.page - 1) * PER_PAGE + 1;
    let last = (shelf.page * PER_PAGE).min(shelf.total);
    let _ = write!(m, "<nav class=pager aria-label=Pages>");
    if shelf.page > 1 {
        let _ = write!(
            m,
            "<a href=\"{base}{}\" rel=prev>‹ previous</a>",
            shelf.page - 1
        );
    }
    let _ = write!(
        m,
        "<span class=next>{first}–{last} of {} · page {}/{pages}",
        shelf.total, shelf.page
    );
    if shelf.page < pages {
        let _ = write!(
            m,
            " · <a href=\"{base}{}\" rel=next>next ›</a>",
            shelf.page + 1
        );
    }
    m.push_str("</span></nav>");
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
        let _ = write!(m, "<a class=card href=\"/r/{}/1\">", book.id);
        cover(&mut m, c.as_ref(), shade(series.id), &book.name);
        let _ = write!(
            m,
            "<b>{}</b><span>{}</span></a>",
            escape(&book.name),
            plural(book.page_count, "page", "pages")
        );
    }
    m.push_str("</div>");
    let here = Here {
        library: Some(library),
        collections,
        collection,
        go: "library",
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
    };
    shell("Library", &[], &here, main)
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
    fn escapes_markup() {
        assert_eq!(
            escape(r#"<a href="x">Mirelle's & co</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Mirelle&#39;s &amp; co&lt;/a&gt;"
        );
    }
}
