//! Server-rendered pages: the library, a series and the bare reader.
//!
//! Plain HTML with no JavaScript and a few lines of CSS. The real design
//! (docs/design) arrives with the library UI milestone; these pages only
//! have to work, stay small and read front to back.

use std::fmt::Write;

use crate::catalog::{Book, Page, Series};
use crate::libraries::{self, Library};

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

const STYLE: &str = "body{font-family:system-ui,sans-serif;max-width:48rem;margin:1rem auto;padding:0 1rem}\
a{color:inherit}li{margin:.25rem 0}\
.reader{max-width:none;margin:0;padding:0;background:#000;color:#ccc;text-align:center}\
.reader img{display:block;margin:0 auto;max-width:100%;max-height:100vh}\
.reader nav{padding:.5rem}";

fn document(title: &str, body_class: &str, head_extra: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=en><head><meta charset=utf-8>\
         <meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{}</title><style>{STYLE}</style>{head_extra}</head>\
         <body class=\"{body_class}\">{body}</body></html>\n",
        escape(title)
    )
}

/// The line of links to every library and the settings, on library pages.
/// `current` is the library shown, if any.
fn libraries_nav(libraries: &[Library], current: Option<&Library>) -> String {
    let mut nav = String::from("<nav>");
    for l in libraries {
        let here = current.is_some_and(|c| c.folder == l.folder);
        let _ = write!(
            nav,
            "<a href=\"{}\"{}>{}</a> · ",
            l.url(),
            if here { " aria-current=page" } else { "" },
            escape(&l.name)
        );
    }
    nav.push_str("<a href=\"/settings\">Settings</a></nav>");
    nav
}

fn series_item(body: &mut String, s: &Series, books: i64) {
    let plural = if books == 1 { "book" } else { "books" };
    let _ = write!(
        body,
        "<li><a href=\"/s/{}\">{}</a> · {books} {plural}</li>",
        s.id,
        escape(&s.name)
    );
}

/// A library's page: its series, those in collections grouped under each
/// collection's name. `current` is `None` when there are no books at all.
pub fn library(
    libraries: &[Library],
    current: Option<&Library>,
    series: &[(Series, i64)],
) -> String {
    let Some(library) = current else {
        let body = "<h1>Library</h1>\
            <p>No books yet. The worker adds them when it scans the library.</p>";
        return document("Library · Koma-1", "", "", body);
    };
    let mut body = libraries_nav(libraries, current);
    let _ = write!(body, "<h1>{}</h1>", escape(&library.name));
    // Series on their own first, then each collection in order of its name.
    let mut collections: Vec<(&str, Vec<&(Series, i64)>)> = Vec::new();
    body.push_str("<ul>");
    for item in series {
        match libraries::collection_of(&item.0.path) {
            None => series_item(&mut body, &item.0, item.1),
            Some(c) => match collections.iter_mut().find(|(name, _)| *name == c) {
                Some((_, list)) => list.push(item),
                None => collections.push((c, vec![item])),
            },
        }
    }
    body.push_str("</ul>");
    collections.sort_by(|a, b| crate::natsort::compare_paths(a.0, b.0));
    for (name, list) in collections {
        let _ = write!(body, "<h2>{}</h2><ul>", escape(name));
        for (s, books) in list {
            series_item(&mut body, s, *books);
        }
        body.push_str("</ul>");
    }
    document(&format!("{} · Koma-1", library.name), "", "", &body)
}

/// The libraries and their settings, read-only until accounts (M5).
pub fn settings(libraries: &[Library]) -> String {
    let mut body = libraries_nav(libraries, None);
    body.push_str(
        "<h1>Settings</h1><p>Libraries and their settings. They come from the folders in \
         /library and can be changed once accounts exist.</p>\
         <table><thead><tr><th>Library<th>Folder<th>Type<th>Direction<th>Colour<th>Series\
         </thead><tbody>",
    );
    for l in libraries {
        let folder = if l.folder.is_empty() {
            "/library".to_owned()
        } else {
            format!("/library/{}", l.folder)
        };
        let _ = write!(
            body,
            "<tr><td><a href=\"{}\">{}</a><td>{}<td>{}<td>{}<td>{}<td>{}",
            l.url(),
            escape(&l.name),
            escape(&folder),
            l.kind.as_str(),
            l.direction.as_str(),
            l.colour.0,
            l.series
        );
    }
    body.push_str("</tbody></table>");
    document("Settings · Koma-1", "", "", &body)
}

pub fn series(series: &Series, books: &[Book]) -> String {
    let mut body = format!(
        "<p><a href=\"/\">Library</a></p><h1>{}</h1><ol>",
        escape(&series.name)
    );
    for book in books {
        let _ = write!(
            body,
            "<li><a href=\"/r/{}/1\">{}</a> · {} pages</li>",
            book.id,
            escape(&book.name),
            book.page_count
        );
    }
    body.push_str("</ol>");
    document(&format!("{} · Koma-1", series.name), "", "", &body)
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
    document(&title, "reader", &head, &body)
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

    fn series(id: i64, path: &str, name: &str) -> Series {
        Series {
            id,
            path: path.into(),
            name: name.into(),
        }
    }

    #[test]
    fn library_escapes_names() {
        let libs = Library::all(vec![("".into(), 1)]);
        let html = library(&libs, libs.first(), &[(series(3, "x", "<Starfall>"), 2)]);
        assert!(html.contains("<a href=\"/s/3\">&lt;Starfall&gt;</a> · 2 books"));
        assert!(!html.contains("<Starfall>"));
    }

    #[test]
    fn library_groups_collections_after_lone_series() {
        let libs = Library::all(vec![("".into(), 1), ("manga".into(), 3)]);
        let html = library(
            &libs,
            libs.get(1),
            &[
                (series(1, "manga/Saga/Moonforge", "Moonforge"), 1),
                (series(2, "manga/Starfall", "Starfall"), 2),
                (series(3, "manga/Saga/Starfall Zero", "Starfall Zero"), 1),
            ],
        );
        let at = |s: &str| html.find(s).unwrap();
        assert!(html.contains("<a href=\"/l/manga\" aria-current=page>Manga</a>"));
        assert!(html.contains("<a href=\"/\">Library</a>"));
        assert!(at(">Starfall<") < at("<h2>Saga</h2>"));
        assert!(at("<h2>Saga</h2>") < at(">Moonforge<"));
        assert!(at(">Moonforge<") < at(">Starfall Zero<"));
    }

    #[test]
    fn settings_list_every_library() {
        let libs = Library::all(vec![("".into(), 2), ("comics".into(), 1)]);
        let html = settings(&libs);
        assert!(html.contains("<td>/library<td>manga<td>right to left<td>amber<td>2"));
        assert!(html.contains("<td>/library/comics<td>comics<td>left to right<td>teal<td>1"));
    }
}
