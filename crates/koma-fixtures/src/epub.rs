//! Made-up EPUB books: an EPUB 3 package with a navigation document (or an
//! EPUB 2 NCX), XHTML chapters, a stylesheet, and a cover picture.

use crate::zip::{Compression, ZipWriter};

/// A made-up EPUB to build.
pub struct Epub<'a> {
    pub title: &'a str,
    pub author: &'a str,
    /// `(title, body)`: each chapter's title (in the contents) and the
    /// XHTML of its `<body>`.
    pub chapters: Vec<(&'a str, String)>,
    /// A PNG cover, named as the cover in the package.
    pub cover: Option<Vec<u8>>,
    /// More pictures, at `images/<name>`.
    pub pictures: Vec<(&'a str, Vec<u8>)>,
    /// EPUB 2: contents in an NCX, no navigation document.
    pub ncx: bool,
    /// Fixed layout (`rendition:layout` `pre-paginated`).
    pub fixed_layout: bool,
}

impl<'a> Epub<'a> {
    pub fn new(title: &'a str, author: &'a str) -> Self {
        Self {
            title,
            author,
            chapters: Vec::new(),
            cover: None,
            pictures: Vec::new(),
            ncx: false,
            fixed_layout: false,
        }
    }

    pub fn chapter(mut self, title: &'a str, body: &str) -> Self {
        self.chapters.push((title, body.to_owned()));
        self
    }

    /// The bytes of the `.epub` file.
    pub fn build(&self) -> Vec<u8> {
        let mut z = ZipWriter::new();
        // The mimetype comes first, stored, as the format asks.
        z.add("mimetype", b"application/epub+zip", Compression::Stored);
        z.add(
            "META-INF/container.xml",
            br#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#,
            Compression::Deflated,
        );
        let mut manifest = String::new();
        let mut spine = String::new();
        let mut nav = String::new();
        let mut ncx = String::new();
        for (n, (title, body)) in self.chapters.iter().enumerate() {
            let n = n + 1;
            manifest.push_str(&format!(
                "<item id=\"c{n}\" href=\"text/ch{n:02}.xhtml\" media-type=\"application/xhtml+xml\"/>\n"
            ));
            spine.push_str(&format!("<itemref idref=\"c{n}\"/>"));
            nav.push_str(&format!(
                "<li><a href=\"text/ch{n:02}.xhtml\">{}</a></li>\n",
                escape(title)
            ));
            ncx.push_str(&format!(
                "<navPoint id=\"p{n}\" playOrder=\"{n}\"><navLabel><text>{}</text></navLabel>\
                 <content src=\"text/ch{n:02}.xhtml\"/></navPoint>\n",
                escape(title)
            ));
            let doc = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE html>\n\
                 <html xmlns=\"http://www.w3.org/1999/xhtml\" xmlns:epub=\"http://www.idpf.org/2007/ops\">\
                 <head><title>{}</title><link rel=\"stylesheet\" href=\"../style.css\"/></head>\
                 <body>{body}</body></html>",
                escape(title)
            );
            z.add(
                &format!("OEBPS/text/ch{n:02}.xhtml"),
                doc.as_bytes(),
                Compression::Deflated,
            );
        }
        if let Some(cover) = &self.cover {
            manifest.push_str(
                "<item id=\"cover\" href=\"images/cover.png\" media-type=\"image/png\" properties=\"cover-image\"/>\n",
            );
            z.add("OEBPS/images/cover.png", cover, Compression::Stored);
        }
        for (i, (name, bytes)) in self.pictures.iter().enumerate() {
            manifest.push_str(&format!(
                "<item id=\"pic{i}\" href=\"images/{name}\" media-type=\"image/png\"/>\n"
            ));
            z.add(
                &format!("OEBPS/images/{name}"),
                bytes,
                Compression::Deflated,
            );
        }
        manifest.push_str("<item id=\"css\" href=\"style.css\" media-type=\"text/css\"/>\n");
        z.add(
            "OEBPS/style.css",
            b"p { text-indent: 1em; color: #333 }",
            Compression::Deflated,
        );
        if self.ncx {
            manifest.push_str(
                "<item id=\"ncx\" href=\"toc.ncx\" media-type=\"application/x-dtbncx+xml\"/>\n",
            );
            let doc = format!(
                "<?xml version=\"1.0\"?><ncx xmlns=\"http://www.daisy.org/z3986/2005/ncx/\" version=\"2005-1\">\
                 <navMap>{ncx}</navMap></ncx>"
            );
            z.add("OEBPS/toc.ncx", doc.as_bytes(), Compression::Deflated);
        } else {
            manifest.push_str(
                "<item id=\"nav\" href=\"nav.xhtml\" media-type=\"application/xhtml+xml\" properties=\"nav\"/>\n",
            );
            let doc = format!(
                "<?xml version=\"1.0\"?><html xmlns=\"http://www.w3.org/1999/xhtml\" \
                 xmlns:epub=\"http://www.idpf.org/2007/ops\"><head><title>Contents</title></head><body>\
                 <nav epub:type=\"toc\"><h1>Contents</h1><ol>{nav}</ol></nav></body></html>"
            );
            z.add("OEBPS/nav.xhtml", doc.as_bytes(), Compression::Deflated);
        }
        let layout = if self.fixed_layout {
            "<meta property=\"rendition:layout\">pre-paginated</meta>"
        } else {
            ""
        };
        let toc = if self.ncx { " toc=\"ncx\"" } else { "" };
        let opf = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <package xmlns=\"http://www.idpf.org/2007/opf\" version=\"3.0\" unique-identifier=\"id\">\
             <metadata xmlns:dc=\"http://purl.org/dc/elements/1.1/\">\
             <dc:identifier id=\"id\">urn:uuid:00000000-0000-0000-0000-000000000000</dc:identifier>\
             <dc:title>{}</dc:title><dc:creator>{}</dc:creator><dc:language>en</dc:language>{layout}\
             </metadata><manifest>\n{manifest}</manifest><spine{toc}>{spine}</spine></package>",
            escape(self.title),
            escape(self.author)
        );
        z.add("OEBPS/content.opf", opf.as_bytes(), Compression::Deflated);
        z.finish()
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
