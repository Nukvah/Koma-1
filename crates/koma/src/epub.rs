//! EPUB books: what a reflowable EPUB holds, read once at ingest.
//!
//! The container (`META-INF/container.xml`) names the package (OPF): its
//! metadata, its files (manifest) and their reading order (spine). The
//! contents come from the EPUB 3 navigation document, else the EPUB 2 NCX.
//!
//! Each chapter is rewritten by [`clean`] from an allowlist. Only markup
//! that carries meaning (headings, emphasis, lists, tables, images, ruby,
//! links) is written back, by this code, with every text and attribute
//! escaped; scripts, styles, forms, embedded objects and every attribute not
//! listed are dropped. What the reader shows is written here, never copied.
//! The book's own CSS and fonts are not kept: every book reads in the
//! reader's typography.
//!
//! Everything here takes hostile bytes: bad input gives an error or less,
//! never a panic. Fixed-layout books are refused (they come in M11).

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write;

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use crate::meta::{Direction, Meta};

/// XML documents (container, package, contents, chapters) are read up to
/// this size; a bigger one is an error, or a chapter left out.
pub const MAX_XML_BYTES: u64 = 16 * 1024 * 1024;

/// Elements nested deeper than this are unwrapped (their text is kept).
const MAX_DEPTH: usize = 256;

/// A file of the package.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    /// Its path in the archive.
    pub path: String,
    pub media_type: String,
    /// EPUB 3 `properties` (`nav`, `cover-image`…).
    pub properties: String,
}

/// The package: metadata, files and reading order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Package {
    pub meta: Meta,
    pub items: Vec<Item>,
    /// Paths of the chapters in reading order, each once.
    pub spine: Vec<String>,
    /// The EPUB 3 navigation document, if any.
    pub nav: Option<String>,
    /// The EPUB 2 NCX, if any.
    pub ncx: Option<String>,
    /// The cover image's path, if the package names one.
    pub cover: Option<String>,
    /// `rendition:layout` is `pre-paginated`: pages, not text.
    pub fixed_layout: bool,
}

impl Package {
    /// The images of the book (JPEG, PNG, GIF, WebP), the cover first.
    pub fn images(&self) -> Vec<&str> {
        let is_image = |i: &&Item| {
            matches!(
                i.media_type.as_str(),
                "image/jpeg" | "image/png" | "image/gif" | "image/webp"
            )
        };
        let mut out: Vec<&str> = Vec::new();
        if let Some(cover) = &self.cover
            && self.items.iter().filter(is_image).any(|i| &i.path == cover)
        {
            out.push(cover);
        }
        for item in self.items.iter().filter(is_image) {
            if !out.contains(&item.path.as_str()) {
                out.push(&item.path);
            }
        }
        out
    }
}

/// One entry of the contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TocEntry {
    /// 1 for the top level.
    pub level: u32,
    pub title: String,
    /// The chapter's path in the archive.
    pub path: String,
    /// The place in it (`id`), as the book writes it.
    pub anchor: Option<String>,
}

/// The package's path, from `META-INF/container.xml`.
pub fn rootfile(container: &[u8]) -> Option<String> {
    let text = decode(container);
    let mut r = reader(&text);
    loop {
        match r.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if is(&e, "rootfile") => {
                let path = attr(&e, "full-path")?;
                return normalize(&percent_decode(&path));
            }
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Reads the package document at `path`.
pub fn package(path: &str, bytes: &[u8]) -> Package {
    let dir = parent(path);
    let text = decode(bytes);
    let mut r = reader(&text);
    let mut p = Package::default();
    let mut ids: Vec<String> = Vec::new();
    let mut toc_id = None;
    let mut cover_id = None;
    // The element whose text is being read: its name and `property`/`id`.
    let mut field: Option<(String, String)> = None;
    let mut text_buf = String::new();
    let mut in_metadata = false;
    let mut series_id = None;
    loop {
        let event = match r.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(e) => e,
        };
        match event {
            Event::Start(e) | Event::Empty(e) if is(&e, "metadata") => in_metadata = true,
            Event::Start(e) if in_metadata && field.is_none() => {
                let name = local(&e);
                let what = match name.as_str() {
                    "meta" => attr(&e, "property").unwrap_or_default(),
                    _ => String::new(),
                };
                if name == "meta" && what == "belongs-to-collection" {
                    series_id = attr(&e, "id");
                }
                if name == "meta" && what == "group-position" {
                    let refines = attr(&e, "refines").unwrap_or_default();
                    let ours = series_id
                        .as_deref()
                        .is_some_and(|id| refines == format!("#{id}"));
                    field = Some((if ours { "group-position" } else { "" }.into(), what));
                } else {
                    field = Some((name, what));
                }
                text_buf.clear();
            }
            Event::Empty(e) if in_metadata && is(&e, "meta") => {
                // EPUB 2: <meta name="cover" content="id"/>, and Calibre's
                // series.
                let content = attr(&e, "content").unwrap_or_default();
                match attr(&e, "name").as_deref() {
                    Some("cover") => cover_id = Some(content),
                    Some("calibre:series") if p.meta.series.is_empty() => {
                        p.meta.series = content.trim().to_owned();
                    }
                    Some("calibre:series_index") if p.meta.volume.is_empty() => {
                        p.meta.volume = number_text(&content);
                    }
                    _ => {}
                }
            }
            Event::Text(t) if field.is_some() => text_buf.push_str(&t.xml10_content()),
            Event::CData(t) if field.is_some() => text_buf.push_str(&t.xml10_content()),
            Event::GeneralRef(r) if field.is_some() => push_ref(&mut text_buf, &r),
            Event::End(_) if field.is_some() => {
                let (name, what) = field.take().unwrap_or_default();
                let value = collapse(&text_buf);
                text_buf.clear();
                if value.is_empty() {
                    continue;
                }
                let m = &mut p.meta;
                match name.as_str() {
                    "title" if m.title.is_empty() => m.title = value,
                    "creator" => m.authors.push(value),
                    "publisher" => m.publishers.push(value),
                    "subject" => m.tags.push(value),
                    "language" if m.language.is_empty() => m.language = value,
                    "description" if m.summary.is_empty() => m.summary = strip_tags(&value),
                    "date" if m.year.is_none() => m.year = year_of(&value),
                    "meta" if what == "rendition:layout" => {
                        p.fixed_layout |= value == "pre-paginated";
                    }
                    "meta" if what == "belongs-to-collection" && m.series.is_empty() => {
                        m.series = value;
                    }
                    "group-position" if m.volume.is_empty() => m.volume = number_text(&value),
                    _ => {}
                }
            }
            Event::End(e) if is_end(&e, "metadata") => in_metadata = false,
            Event::Start(e) | Event::Empty(e) if is(&e, "item") => {
                let (Some(id), Some(href)) = (attr(&e, "id"), attr(&e, "href")) else {
                    continue;
                };
                let Some((path, _)) = resolve(&dir, &href) else {
                    continue;
                };
                let properties = attr(&e, "properties").unwrap_or_default();
                let media_type = attr(&e, "media-type")
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if properties.split_whitespace().any(|w| w == "nav") {
                    p.nav = Some(path.clone());
                }
                if properties.split_whitespace().any(|w| w == "cover-image") {
                    p.cover = Some(path.clone());
                }
                p.items.push(Item {
                    id,
                    path,
                    media_type,
                    properties,
                });
            }
            Event::Start(e) | Event::Empty(e) if is(&e, "spine") => {
                toc_id = attr(&e, "toc");
                if attr(&e, "page-progression-direction").as_deref() == Some("rtl") {
                    p.meta.direction = Some(Direction::RightToLeft);
                }
            }
            Event::Start(e) | Event::Empty(e) if is(&e, "itemref") => {
                if let Some(id) = attr(&e, "idref") {
                    ids.push(id);
                }
            }
            _ => {}
        }
    }
    let by_id = |id: &str| p.items.iter().find(|i| i.id == id);
    let mut spine = Vec::new();
    for id in &ids {
        if let Some(item) = by_id(id)
            && is_chapter_type(&item.media_type)
            && !spine.contains(&item.path)
        {
            spine.push(item.path.clone());
        }
    }
    let ncx = toc_id
        .as_deref()
        .and_then(by_id)
        .or_else(|| {
            p.items
                .iter()
                .find(|i| i.media_type == "application/x-dtbncx+xml")
        })
        .map(|i| i.path.clone());
    if p.cover.is_none() {
        p.cover = cover_id.as_deref().and_then(by_id).map(|i| i.path.clone());
    }
    p.spine = spine;
    p.ncx = ncx;
    p
}

fn is_chapter_type(media_type: &str) -> bool {
    matches!(media_type, "application/xhtml+xml" | "text/html")
}

/// The contents from an EPUB 3 navigation document at `path`: its
/// `epub:type="toc"` list (else its first), each link one entry.
pub fn nav(path: &str, bytes: &[u8]) -> Vec<TocEntry> {
    let dir = parent(path);
    let text = decode(bytes);
    let mut out = Vec::new();
    for want_toc in [true, false] {
        let mut r = reader(&text);
        let mut in_nav = 0usize; // nav nesting while inside the chosen one
        let mut depth = 0u32; // list nesting
        let mut link: Option<(String, String)> = None; // href, text
        loop {
            let event = match r.read_event() {
                Ok(Event::Eof) | Err(_) => break,
                Ok(e) => e,
            };
            match event {
                Event::Start(e) if is(&e, "nav") => {
                    let toc =
                        attr(&e, "type").is_some_and(|t| t.split_whitespace().any(|w| w == "toc"));
                    if in_nav > 0 {
                        in_nav += 1;
                    } else if (toc || !want_toc) && out.is_empty() {
                        in_nav = 1;
                    }
                }
                Event::End(e) if is_end(&e, "nav") && in_nav > 0 => {
                    in_nav -= 1;
                    if in_nav == 0 {
                        break;
                    }
                }
                Event::Start(e) if in_nav > 0 && is(&e, "ol") => depth += 1,
                Event::End(e) if in_nav > 0 && is_end(&e, "ol") => depth = depth.saturating_sub(1),
                Event::Start(e) if in_nav > 0 && is(&e, "a") => {
                    link = attr(&e, "href").map(|h| (h, String::new()));
                }
                Event::Text(t) => {
                    if let Some((_, s)) = link.as_mut() {
                        s.push_str(&t.xml10_content());
                    }
                }
                Event::GeneralRef(r) => {
                    if let Some((_, s)) = link.as_mut() {
                        push_ref(s, &r);
                    }
                }
                Event::End(e) if is_end(&e, "a") => {
                    if let Some((href, title)) = link.take() {
                        push_entry(&mut out, &dir, &href, &title, depth.max(1));
                    }
                }
                _ => {}
            }
        }
        if !out.is_empty() {
            break;
        }
    }
    out
}

/// The contents from an EPUB 2 NCX at `path`: its navigation points.
pub fn ncx(path: &str, bytes: &[u8]) -> Vec<TocEntry> {
    let dir = parent(path);
    let text = decode(bytes);
    let mut r = reader(&text);
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut in_map = false;
    let mut label: Option<String> = None;
    let mut reading = false;
    loop {
        let event = match r.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(e) => e,
        };
        match event {
            Event::Start(e) if is(&e, "navMap") => in_map = true,
            Event::End(e) if is_end(&e, "navMap") => in_map = false,
            Event::Start(e) if in_map && is(&e, "navPoint") => {
                depth += 1;
                label = None;
            }
            Event::End(e) if in_map && is_end(&e, "navPoint") => depth = depth.saturating_sub(1),
            Event::Start(e) if in_map && is(&e, "text") => {
                reading = true;
                label = Some(String::new());
            }
            Event::End(e) if is_end(&e, "text") => reading = false,
            Event::Text(t) if reading => {
                if let Some(s) = label.as_mut() {
                    s.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) if reading => {
                if let Some(s) = label.as_mut() {
                    push_ref(s, &r);
                }
            }
            Event::Start(e) | Event::Empty(e) if in_map && is(&e, "content") => {
                if let (Some(src), Some(title)) = (attr(&e, "src"), label.take()) {
                    push_entry(&mut out, &dir, &src, &title, depth.max(1));
                }
            }
            _ => {}
        }
    }
    out
}

fn push_entry(out: &mut Vec<TocEntry>, dir: &str, href: &str, title: &str, level: u32) {
    let title = collapse(title);
    let Some((path, anchor)) = resolve(dir, href) else {
        return;
    };
    if title.is_empty() {
        return;
    }
    out.push(TocEntry {
        level: level.min(6),
        title,
        path,
        anchor,
    });
}

/// A picture a chapter shows: its URL and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Where a chapter's links and pictures lead.
pub struct Links<'a> {
    /// The chapter's own path in the archive.
    pub path: &'a str,
    /// Chapters by path: their 1-based number.
    pub chapters: &'a HashMap<String, usize>,
    /// Pictures by path.
    pub pictures: &'a HashMap<String, Picture>,
}

/// What an element becomes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Written as this element.
    Keep(&'static str),
    /// Left out, its children kept.
    Unwrap,
    /// Left out with everything in it.
    Drop,
}

/// The allowlist: elements written back, by their (lower-case) local name.
fn kind(name: &str) -> Kind {
    const KEEP: &[&str] = &[
        "p",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "em",
        "i",
        "strong",
        "b",
        "u",
        "s",
        "small",
        "sub",
        "sup",
        "br",
        "hr",
        "blockquote",
        "q",
        "cite",
        "code",
        "pre",
        "ul",
        "ol",
        "li",
        "dl",
        "dt",
        "dd",
        "table",
        "thead",
        "tbody",
        "tfoot",
        "tr",
        "th",
        "td",
        "caption",
        "figure",
        "figcaption",
        "ruby",
        "rt",
        "rp",
        "rb",
        "abbr",
        "dfn",
        "mark",
        "del",
        "ins",
        "var",
        "kbd",
        "samp",
        "time",
        "wbr",
        "bdi",
        "bdo",
        "div",
        "aside",
        "a",
        "img",
    ];
    match name {
        "section" | "article" | "header" | "footer" | "main" | "center" => Kind::Keep("div"),
        "head" | "script" | "style" | "title" | "noscript" | "template" | "iframe" | "object"
        | "embed" | "form" | "button" | "select" | "textarea" | "input" | "canvas" | "audio"
        | "video" | "math" | "link" | "meta" | "base" | "frameset" | "frame" | "applet"
        | "dialog" | "slot" | "portal" => Kind::Drop,
        _ => match KEEP.iter().find(|k| **k == name) {
            Some(k) => Kind::Keep(k),
            None => Kind::Unwrap,
        },
    }
}

fn is_void(name: &str) -> bool {
    matches!(name, "br" | "hr" | "img" | "wbr")
}

/// One element open in the input, and what was written for it.
struct Open {
    /// Its local name in the input.
    name: String,
    /// The tag written for it, to close; `None` if nothing was written.
    wrote: Option<&'static str>,
}

/// Rewrites a chapter (XHTML) as the reader shows it: its body, from the
/// allowlist only. Ids become `e-<id>`, so they cannot clash with the
/// reader's own; links to other chapters become their numbers (relative to
/// `/e/<book>/`), links to pictures and pictures themselves their URLs;
/// links elsewhere in the web keep `http(s):`/`mailto:` only.
pub fn clean(bytes: &[u8], links: &Links<'_>) -> String {
    let text = decode(bytes);
    let mut r = reader(&text);
    let dir = parent(links.path);
    let here = links.chapters.get(links.path).copied();
    let mut out = String::with_capacity(text.len() / 2);
    let mut open: Vec<Open> = Vec::new();
    // Inside a dropped element: the depth of `open` it started at.
    let mut dropping: Option<usize> = None;
    // Inside <svg>: only its <image>s are kept, as pictures.
    let mut svg: Option<usize> = None;
    let mut body_seen = false;
    loop {
        let event = match r.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(e) => e,
        };
        match event {
            Event::Start(e) => {
                let name = local(&e);
                if name == "body" {
                    body_seen = true;
                }
                let wrote = if dropping.is_some() {
                    None
                } else if svg.is_some() || name == "svg" {
                    if svg.is_none() {
                        svg = Some(open.len());
                    }
                    if name == "image" {
                        picture(&mut out, &e, &dir, links);
                    }
                    None
                } else if is_hidden(&e) {
                    dropping = Some(open.len());
                    None
                } else {
                    match kind(&name) {
                        Kind::Drop => {
                            dropping = Some(open.len());
                            None
                        }
                        Kind::Unwrap => None,
                        Kind::Keep(_) if open.len() >= MAX_DEPTH => None,
                        Kind::Keep(tag) => element(&mut out, tag, &e, &dir, here, links),
                    }
                };
                open.push(Open { name, wrote });
            }
            Event::Empty(e) => {
                if dropping.is_some() {
                    continue;
                }
                let name = local(&e);
                if svg.is_some() || name == "svg" {
                    if name == "image" {
                        picture(&mut out, &e, &dir, links);
                    }
                    continue;
                }
                if is_hidden(&e) {
                    continue;
                }
                if let Kind::Keep(tag) = kind(&name)
                    && let Some(tag) = element(&mut out, tag, &e, &dir, here, links)
                {
                    let _ = write!(out, "</{tag}>");
                }
            }
            Event::End(e) => {
                let name = e.local_name().as_ref().to_ascii_lowercase();
                // Close up to the nearest element of that name; an end tag
                // that matches nothing open is ignored.
                let Some(at) = open.iter().rposition(|o| o.name == name) else {
                    continue;
                };
                while open.len() > at {
                    let o = open.pop().expect("open is longer than at");
                    if let Some(tag) = o.wrote {
                        let _ = write!(out, "</{tag}>");
                    }
                }
                if dropping.is_some_and(|d| d >= open.len()) {
                    dropping = None;
                }
                if svg.is_some_and(|d| d >= open.len()) {
                    svg = None;
                }
            }
            Event::Text(t) if dropping.is_none() && svg.is_none() => {
                if body_seen || !open.is_empty() {
                    escape_text(&mut out, &t.xml10_content());
                }
            }
            Event::CData(t) if dropping.is_none() && svg.is_none() => {
                escape_text(&mut out, &t.xml10_content());
            }
            Event::GeneralRef(g) if dropping.is_none() && svg.is_none() => {
                let mut s = String::new();
                push_ref(&mut s, &g);
                escape_text(&mut out, &s);
            }
            _ => {}
        }
    }
    while let Some(o) = open.pop() {
        if let Some(tag) = o.wrote {
            let _ = write!(out, "</{tag}>");
        }
    }
    out.trim().to_owned()
}

/// Writes the opening tag of an allowed element with its allowed
/// attributes; returns the tag to close, or `None` for a void element or
/// one left out (a picture that is not in the book).
fn element(
    out: &mut String,
    tag: &'static str,
    e: &BytesStart<'_>,
    dir: &str,
    here: Option<usize>,
    links: &Links<'_>,
) -> Option<&'static str> {
    if tag == "img" {
        picture(out, e, dir, links);
        return None;
    }
    let mut a = String::new();
    if let Some(id) = attr(e, "id") {
        let _ = write!(a, " id=\"e-{}\"", escape_attr(&id));
    }
    if let Some(lang) = attr(e, "lang") {
        let lang: String = lang
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(35)
            .collect();
        if !lang.is_empty() {
            let _ = write!(a, " lang=\"{lang}\"");
        }
    }
    if let Some(d) = attr(e, "dir")
        && matches!(d.as_str(), "ltr" | "rtl" | "auto")
    {
        let _ = write!(a, " dir=\"{d}\"");
    }
    match tag {
        "td" | "th" => {
            for name in ["colspan", "rowspan"] {
                if let Some(n) = attr(e, name).and_then(|v| v.trim().parse::<u16>().ok())
                    && (1..=1000).contains(&n)
                {
                    let _ = write!(a, " {name}=\"{n}\"");
                }
            }
        }
        "ol" => {
            if let Some(n) = attr(e, "start").and_then(|v| v.trim().parse::<i32>().ok()) {
                let _ = write!(a, " start=\"{n}\"");
            }
            if let Some(t) = attr(e, "type")
                && matches!(t.as_str(), "1" | "a" | "A" | "i" | "I")
            {
                let _ = write!(a, " type=\"{t}\"");
            }
        }
        "a" => {
            if let Some(href) = attr(e, "href").and_then(|h| link(&h, dir, here, links)) {
                let _ = write!(a, " href=\"{}\"", escape_attr(&href));
                if href.contains(':') {
                    a.push_str(" rel=\"noreferrer noopener\" target=\"_blank\"");
                }
            }
        }
        _ => {}
    }
    let _ = write!(out, "<{tag}{a}>");
    (!is_void(tag)).then_some(tag)
}

/// Where a link in a chapter leads, as the reader serves it.
fn link(href: &str, dir: &str, here: Option<usize>, links: &Links<'_>) -> Option<String> {
    let href = href.trim();
    if let Some(frag) = href.strip_prefix('#') {
        return Some(format!("#e-{}", percent_decode(frag)));
    }
    if let Some((scheme, _)) = href.split_once(':') {
        let scheme = scheme.to_ascii_lowercase();
        let ok = matches!(scheme.as_str(), "http" | "https" | "mailto")
            && !href.chars().any(|c| c.is_control() || c.is_whitespace());
        return ok.then(|| href.to_owned());
    }
    let (path, anchor) = resolve(dir, href)?;
    let frag = anchor.map(|a| format!("#e-{a}")).unwrap_or_default();
    if let Some(&n) = links.chapters.get(&path) {
        return Some(if Some(n) == here && !frag.is_empty() {
            frag
        } else {
            format!("{n}{frag}")
        });
    }
    links.pictures.get(&path).map(|p| p.url.clone())
}

/// Writes a picture (`<img>`, or an SVG `<image>`) if it is one of the
/// book's; a picture the book lacks is left out.
fn picture(out: &mut String, e: &BytesStart<'_>, dir: &str, links: &Links<'_>) {
    let Some(src) = attr(e, "src").or_else(|| attr(e, "href")) else {
        return;
    };
    let Some(p) = resolve(dir, &src).and_then(|(path, _)| links.pictures.get(&path)) else {
        return;
    };
    let alt = attr(e, "alt").unwrap_or_default();
    let _ = write!(
        out,
        "<img src=\"{}\" alt=\"{}\"",
        escape_attr(&p.url),
        escape_attr(&collapse(&alt))
    );
    if let (Some(w), Some(h)) = (p.width, p.height) {
        let _ = write!(out, " width=\"{w}\" height=\"{h}\"");
    }
    out.push_str(" loading=\"lazy\">");
}

/// `hidden` content is not shown (EPUB 3 uses it for page lists and
/// landmarks in the navigation document).
fn is_hidden(e: &BytesStart<'_>) -> bool {
    attr(e, "hidden").is_some()
}

fn escape_text(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // Control characters are not text.
            c if c.is_control() && !matches!(c, '\n' | '\t') => {}
            c => out.push(c),
        }
    }
}

fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

// --- XML helpers -------------------------------------------------------

/// UTF-8 (without its byte-order mark); invalid bytes become U+FFFD.
fn decode(bytes: &[u8]) -> Cow<'_, str> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    String::from_utf8_lossy(bytes)
}

/// A forgiving reader: end tags need not match, stray `&` is text.
fn reader(text: &str) -> Reader<&[u8]> {
    let mut r = Reader::from_str(text);
    let c = r.config_mut();
    c.check_end_names = false;
    c.allow_unmatched_ends = true;
    c.allow_dangling_amp = true;
    c.check_comments = false;
    r
}

fn local(e: &BytesStart<'_>) -> String {
    e.local_name().as_ref().to_ascii_lowercase()
}

fn is(e: &BytesStart<'_>, name: &str) -> bool {
    e.local_name().as_ref().eq_ignore_ascii_case(name)
}

fn is_end(e: &quick_xml::events::BytesEnd<'_>, name: &str) -> bool {
    e.local_name().as_ref().eq_ignore_ascii_case(name)
}

/// An attribute by local name (any namespace), entities resolved.
fn attr(e: &BytesStart<'_>, name: &str) -> Option<String> {
    for a in e.attributes().with_checks(false).flatten() {
        if a.key.local_name().as_ref().eq_ignore_ascii_case(name) {
            let v = a
                .normalized_value_with(quick_xml::XmlVersion::Implicit1_0, 1, entity)
                .ok()?;
            return Some(v.into_owned());
        }
    }
    None
}

/// A named entity: XML's five, then HTML's.
fn entity(name: &str) -> Option<&'static str> {
    quick_xml::escape::resolve_predefined_entity(name)
        .or_else(|| quick_xml::escape::resolve_html5_entity(name))
}

/// Appends what an entity or character reference stands for; an unknown one
/// stays as written.
fn push_ref(out: &mut String, r: &quick_xml::events::BytesRef<'_>) {
    if r.is_char_ref() {
        if let Ok(Some(c)) = r.resolve_char_ref() {
            out.push(c);
        }
        return;
    }
    let name: &str = r;
    match entity(name) {
        Some(s) => out.push_str(s),
        None => {
            out.push('&');
            out.push_str(name);
            out.push(';');
        }
    }
}

/// Whitespace runs as one space, trimmed.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A description written as HTML, as text: tags cut out.
fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    collapse(&out)
}

fn year_of(s: &str) -> Option<i32> {
    let y: String = s.trim().chars().take(4).collect();
    y.parse().ok().filter(|y| (1000..=9999).contains(y))
}

/// A series position as a number written plainly: `3`, `2.5` (`3.0` → `3`).
fn number_text(s: &str) -> String {
    match s.trim().parse::<f64>() {
        Ok(n) if n.is_finite() && n >= 0.0 => {
            if n.fract() == 0.0 {
                format!("{}", n as i64)
            } else {
                format!("{n}")
            }
        }
        _ => String::new(),
    }
}

// --- Paths -------------------------------------------------------------

/// The folder of an archive path, `""` at the root.
fn parent(path: &str) -> String {
    path.rsplit_once('/')
        .map_or(String::new(), |(d, _)| d.to_owned())
}

/// An archive path from a link in a document in `dir`, and its fragment;
/// `None` for a link out of the archive (with a scheme), or above its root.
pub fn resolve(dir: &str, href: &str) -> Option<(String, Option<String>)> {
    let href = href.trim();
    let (path, frag) = match href.split_once('#') {
        Some((p, f)) => (p, Some(percent_decode(f)).filter(|f| !f.is_empty())),
        None => (href, None),
    };
    let path = path.split_once('?').map_or(path, |(p, _)| p);
    if path.contains(':') {
        return None;
    }
    let path = percent_decode(path);
    let joined = if path.is_empty() {
        return None;
    } else if let Some(abs) = path.strip_prefix('/') {
        abs.to_owned()
    } else if dir.is_empty() {
        path
    } else {
        format!("{dir}/{path}")
    };
    Some((normalize(&joined)?, frag))
}

/// `a/./b/../c` → `a/c`; `None` if it climbs above the root.
fn normalize(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links<'a>(
        path: &'a str,
        chapters: &'a HashMap<String, usize>,
        pictures: &'a HashMap<String, Picture>,
    ) -> Links<'a> {
        Links {
            path,
            chapters,
            pictures,
        }
    }

    fn book_maps() -> (HashMap<String, usize>, HashMap<String, Picture>) {
        let chapters = HashMap::from([
            ("OEBPS/text/one.xhtml".to_owned(), 1),
            ("OEBPS/text/two.xhtml".to_owned(), 2),
        ]);
        let pictures = HashMap::from([(
            "OEBPS/images/map.png".to_owned(),
            Picture {
                url: "/p/abc.png".into(),
                width: Some(600),
                height: Some(400),
            },
        )]);
        (chapters, pictures)
    }

    fn clean_one(body: &str) -> String {
        let (c, p) = book_maps();
        let doc = format!(
            "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\"><head>\
             <title>T</title><style>p{{color:red}}</style></head><body>{body}</body></html>"
        );
        clean(doc.as_bytes(), &links("OEBPS/text/one.xhtml", &c, &p))
    }

    #[test]
    fn keeps_meaning_and_drops_the_rest() {
        assert_eq!(
            clean_one(
                "<h1 class=\"x\" style=\"color:red\">Low Tide</h1><p onclick=\"evil()\">The <i>bell</i> rang.</p>"
            ),
            "<h1>Low Tide</h1><p>The <i>bell</i> rang.</p>"
        );
        assert_eq!(
            clean_one("<p>a<script>alert(1)</script>b<style>x</style>c</p>"),
            "<p>abc</p>"
        );
        assert_eq!(
            clean_one("<div><span class=\"sc\">Mirelle</span> <font>said</font></div>"),
            "<div>Mirelle said</div>"
        );
        assert_eq!(
            clean_one("<section><form><input/>x</form><iframe src=\"//e\">y</iframe>z</section>"),
            "<div>z</div>"
        );
        assert_eq!(
            clean_one("<p><ruby>港<rt>みなと</rt></ruby></p>"),
            "<p><ruby>港<rt>みなと</rt></ruby></p>"
        );
        assert_eq!(
            clean_one("<table><tr><td colspan=\"2\" rowspan=\"x\">a</td></tr></table>"),
            "<table><tr><td colspan=\"2\">a</td></tr></table>"
        );
        assert_eq!(
            clean_one("<p hidden=\"\">secret</p><p>shown</p>"),
            "<p>shown</p>"
        );
    }

    #[test]
    fn escapes_what_it_writes() {
        assert_eq!(
            clean_one("<p>&lt;script&gt; &amp; &nbsp;&mdash;&#x41;&unknown;</p>"),
            "<p>&lt;script&gt; &amp; \u{a0}—A&amp;unknown;</p>"
        );
        assert_eq!(
            clean_one("<p><![CDATA[<b>x</b>]]></p>"),
            "<p>&lt;b&gt;x&lt;/b&gt;</p>"
        );
        assert_eq!(
            clean_one("<p id='a\"b&lt;'>x</p>"),
            "<p id=\"e-a&quot;b&lt;\">x</p>"
        );
    }

    #[test]
    fn links_and_pictures() {
        assert_eq!(
            clean_one(
                "<p><a href=\"two.xhtml#c2\">on</a> <a href=\"one.xhtml#n1\">here</a> <a href=\"#n2\">note</a> <a href=\"one.xhtml\">top</a></p>"
            ),
            "<p><a href=\"2#e-c2\">on</a> <a href=\"#e-n1\">here</a> <a href=\"#e-n2\">note</a> <a href=\"1\">top</a></p>"
        );
        assert_eq!(
            clean_one(
                "<p><a href=\"javascript:alert(1)\">x</a><a href=\"JaVaScRiPt:alert(1)\">y</a><a href=\"data:text/html,x\">z</a><a href=\"missing.xhtml\">w</a></p>"
            ),
            "<p><a>x</a><a>y</a><a>z</a><a>w</a></p>"
        );
        assert_eq!(
            clean_one("<p><a href=\"https://example.org/?a=1&amp;b=2\">site</a></p>"),
            "<p><a href=\"https://example.org/?a=1&amp;b=2\" rel=\"noreferrer noopener\" target=\"_blank\">site</a></p>"
        );
        assert_eq!(
            clean_one(
                "<img src=\"../images/map.png\" alt=\"The &quot;harbor&quot;\" onerror=\"x()\"/><img src=\"../images/none.png\"/>"
            ),
            "<img src=\"/p/abc.png\" alt=\"The &quot;harbor&quot;\" width=\"600\" height=\"400\" loading=\"lazy\">"
        );
        // A cover page drawn as SVG: its image kept, the rest of it not.
        assert_eq!(
            clean_one(
                "<div><svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:xlink=\"http://www.w3.org/1999/xlink\"><script>x</script><text>no</text><image xlink:href=\"../images/map.png\"/></svg></div>"
            ),
            "<div><img src=\"/p/abc.png\" alt=\"\" width=\"600\" height=\"400\" loading=\"lazy\"></div>"
        );
        // Empty anchors stay, so links into them land.
        assert_eq!(clean_one("<a id=\"n1\"/>x"), "<a id=\"e-n1\"></a>x");
    }

    #[test]
    fn broken_markup_still_gives_well_formed_output() {
        assert_eq!(clean_one("<p><b>a</p>b</b>"), "<p><b>a</b></p>b");
        assert_eq!(clean_one("<p>open"), "<p>open</p>");
        assert_eq!(clean_one("</div></p>x"), "x");
        assert_eq!(clean_one("<p>a & b</p>"), "<p>a &amp; b</p>");
        let deep = "<div>".repeat(1000) + "x";
        let out = clean_one(&deep);
        assert_eq!(out.matches("<div>").count(), out.matches("</div>").count());
        assert!(out.matches("<div>").count() <= MAX_DEPTH);
    }

    #[test]
    fn paths() {
        assert_eq!(
            resolve("OEBPS/text", "../images/a%20b.png#x"),
            Some(("OEBPS/images/a b.png".into(), Some("x".into())))
        );
        assert_eq!(
            resolve("", "/OEBPS/c.xhtml"),
            Some(("OEBPS/c.xhtml".into(), None))
        );
        assert_eq!(resolve("a", "../../x"), None);
        assert_eq!(resolve("a", "http://x/y"), None);
        assert_eq!(resolve("a", "#only"), None);
    }

    const OPF: &str = r##"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:opf="http://www.idpf.org/2007/opf">
    <dc:title>The Harbor Fire</dc:title>
    <dc:creator>Sela Marr</dc:creator>
    <dc:language>en</dc:language>
    <dc:subject>Sea</dc:subject>
    <dc:date>2021-04-01</dc:date>
    <dc:description>&lt;p&gt;Smoke over the &lt;b&gt;quay&lt;/b&gt;.&lt;/p&gt;</dc:description>
    <meta property="belongs-to-collection" id="c1">Saltwind</meta>
    <meta refines="#c1" property="group-position">3.0</meta>
    <meta name="cover" content="cov"/>
  </metadata>
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="c1x" href="text/one.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2x" href="text/two.xhtml" media-type="application/xhtml+xml"/>
    <item id="map" href="images/map.png" media-type="image/png"/>
    <item id="cov" href="images/cover.jpg" media-type="image/jpeg"/>
    <item id="css" href="style.css" media-type="text/css"/>
  </manifest>
  <spine toc="ncx" page-progression-direction="rtl">
    <itemref idref="c1x"/><itemref idref="c2x"/><itemref idref="c1x"/><itemref idref="nope"/><itemref idref="css"/>
  </spine>
</package>"##;

    #[test]
    fn reads_the_package() {
        let container = br#"<container><rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles></container>"#;
        assert_eq!(rootfile(container).as_deref(), Some("OEBPS/content.opf"));
        let p = package("OEBPS/content.opf", OPF.as_bytes());
        assert_eq!(p.meta.title, "The Harbor Fire");
        assert_eq!(p.meta.authors, ["Sela Marr"]);
        assert_eq!(p.meta.tags, ["Sea"]);
        assert_eq!(p.meta.year, Some(2021));
        assert_eq!(p.meta.summary, "Smoke over the quay .");
        assert_eq!(p.meta.series, "Saltwind");
        assert_eq!(p.meta.volume, "3");
        assert_eq!(p.meta.direction, Some(Direction::RightToLeft));
        assert_eq!(p.spine, ["OEBPS/text/one.xhtml", "OEBPS/text/two.xhtml"]);
        assert_eq!(p.nav.as_deref(), Some("OEBPS/nav.xhtml"));
        assert_eq!(p.ncx.as_deref(), Some("OEBPS/toc.ncx"));
        assert_eq!(p.cover.as_deref(), Some("OEBPS/images/cover.jpg"));
        assert_eq!(
            p.images(),
            ["OEBPS/images/cover.jpg", "OEBPS/images/map.png"]
        );
        assert!(!p.fixed_layout);
        let fixed = OPF.replace(
            "<meta name=\"cover\" content=\"cov\"/>",
            "<meta property=\"rendition:layout\">pre-paginated</meta>",
        );
        assert!(package("OEBPS/content.opf", fixed.as_bytes()).fixed_layout);
    }

    #[test]
    fn reads_the_contents() {
        let doc = r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body>
<nav epub:type="landmarks" hidden=""><ol><li><a href="text/one.xhtml">Begin</a></li></ol></nav>
<nav epub:type="toc"><ol>
  <li><a href="text/one.xhtml">1 · Low   Tide</a>
    <ol><li><a href="text/one.xhtml#s2">The &amp; Ferry</a></li></ol></li>
  <li><span>Part two</span><ol><li><a href="text/two.xhtml">2 · Ash</a></li></ol></li>
</ol></nav></body></html>"#;
        let toc = nav("OEBPS/nav.xhtml", doc.as_bytes());
        let got: Vec<_> = toc
            .iter()
            .map(|t| {
                (
                    t.level,
                    t.title.as_str(),
                    t.path.as_str(),
                    t.anchor.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (1, "1 · Low Tide", "OEBPS/text/one.xhtml", None),
                (2, "The & Ferry", "OEBPS/text/one.xhtml", Some("s2")),
                (2, "2 · Ash", "OEBPS/text/two.xhtml", None),
            ]
        );
        let ncx_doc = r#"<ncx><navMap>
<navPoint id="a"><navLabel><text>Low Tide</text></navLabel><content src="text/one.xhtml"/>
  <navPoint id="b"><navLabel><text>Ferry</text></navLabel><content src="text/one.xhtml#s2"/></navPoint>
</navPoint>
<navPoint id="c"><navLabel><text>Ash</text></navLabel><content src="text/two.xhtml"/></navPoint>
</navMap></ncx>"#;
        let toc = ncx("OEBPS/toc.ncx", ncx_doc.as_bytes());
        let got: Vec<_> = toc.iter().map(|t| (t.level, t.title.as_str())).collect();
        assert_eq!(got, [(1, "Low Tide"), (2, "Ferry"), (1, "Ash")]);
    }

    #[test]
    fn hostile_bytes_do_not_panic() {
        let (c, p) = book_maps();
        for doc in [
            &b""[..],
            b"\xff\xfe<",
            b"<",
            b"<a href=\"",
            b"<!DOCTYPE x [<!ENTITY a \"b\">]><p>&a;</p>",
            b"<p>&#x0;&#xD800;&#99999999;</p>",
            b"<svg><svg></p></svg>",
        ] {
            let _ = clean(doc, &links("x.xhtml", &c, &p));
            let _ = package("x.opf", doc);
            let _ = nav("x.xhtml", doc);
            let _ = ncx("x.ncx", doc);
            let _ = rootfile(doc);
        }
        assert_eq!(percent_decode("%"), "%");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("%41%zz"), "A%zz");
    }
}
