//! EPUB documents: the container, the package, both kinds of contents, and
//! the chapter cleaner, whose output must stay inside its allowlist.
#![no_main]

use std::collections::HashMap;

use koma::epub;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = epub::rootfile(data);
    let package = epub::package("OEBPS/content.opf", data);
    let _ = package.images();
    let _ = epub::nav("OEBPS/nav.xhtml", data);
    let _ = epub::ncx("OEBPS/toc.ncx", data);
    let chapters = HashMap::from([("OEBPS/text/one.xhtml".to_owned(), 1)]);
    let pictures = HashMap::from([(
        "OEBPS/images/a.png".to_owned(),
        epub::Picture {
            url: "/p/a.png".into(),
            width: Some(10),
            height: Some(10),
        },
    )]);
    let out = epub::clean(
        data,
        &epub::Links {
            path: "OEBPS/text/one.xhtml",
            chapters: &chapters,
            pictures: &pictures,
        },
    );
    // Every tag written is on the allowlist, with only the attributes the
    // cleaner writes; no `javascript:` link survives.
    const TAGS: &[&str] = &[
        "p", "h1", "h2", "h3", "h4", "h5", "h6", "em", "i", "strong", "b", "u", "s", "small",
        "sub", "sup", "br", "hr", "blockquote", "q", "cite", "code", "pre", "ul", "ol", "li",
        "dl", "dt", "dd", "table", "thead", "tbody", "tfoot", "tr", "th", "td", "caption",
        "figure", "figcaption", "ruby", "rt", "rp", "rb", "abbr", "dfn", "mark", "del", "ins",
        "var", "kbd", "samp", "time", "wbr", "bdi", "bdo", "div", "aside", "a", "img",
    ];
    const ATTRS: &[&str] = &[
        "id", "lang", "dir", "colspan", "rowspan", "start", "type", "href", "rel", "target",
        "src", "alt", "width", "height", "loading",
    ];
    for tag in out.split('<').skip(1) {
        let head = tag.split('>').next().unwrap_or("");
        let head = head.strip_prefix('/').unwrap_or(head);
        // Quoted values cut out: what is left is the name and attribute names.
        let mut bare = String::new();
        let mut quoted = false;
        for c in head.chars() {
            if c == '"' {
                quoted = !quoted;
            } else if !quoted {
                bare.push(c);
            }
        }
        assert!(!quoted, "unclosed quote in {out}");
        let mut words = bare.split_whitespace();
        let name = words.next().unwrap_or("");
        assert!(TAGS.contains(&name), "tag {name:?} in {out}");
        for w in words {
            let attr = w.trim_end_matches('=');
            assert!(ATTRS.contains(&attr), "attribute {attr:?} in {out}");
        }
        if let Some(at) = head.find("href=\"") {
            let v = head[at + 6..].to_ascii_lowercase();
            assert!(!v.starts_with("javascript:") && !v.starts_with("data:"), "{out}");
        }
    }
});
