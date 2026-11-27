//! Server-rendered pages: the library, a series and the bare reader.
//!
//! Plain HTML with no JavaScript and a few lines of CSS. The real design
//! (docs/design) arrives with the library UI milestone; these pages only
//! have to work, stay small and read front to back.

use std::fmt::Write;

use crate::catalog::{Book, Page, Series};

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

pub fn library(series: &[(Series, i64)]) -> String {
    let mut body = String::from("<h1>Library</h1>");
    if series.is_empty() {
        body.push_str("<p>No books yet. The worker adds them when it scans the library.</p>");
    } else {
        body.push_str("<ul>");
        for (s, books) in series {
            let plural = if *books == 1 { "book" } else { "books" };
            let _ = write!(
                body,
                "<li><a href=\"/s/{}\">{}</a> · {books} {plural}</li>",
                s.id,
                escape(&s.name)
            );
        }
        body.push_str("</ul>");
    }
    document("Library · Koma-1", "", "", &body)
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

    #[test]
    fn library_escapes_names() {
        let s = Series {
            id: 3,
            path: "x".into(),
            name: "<Starfall>".into(),
        };
        let html = library(&[(s, 2)]);
        assert!(html.contains("<a href=\"/s/3\">&lt;Starfall&gt;</a> · 2 books"));
        assert!(!html.contains("<Starfall>"));
    }
}
