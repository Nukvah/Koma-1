//! Metadata inside books: `ComicInfo.xml` and `metadata.json`, read and
//! merged at ingest.
//!
//! Layers go from weakest to strongest: `ComicInfo.xml`, then
//! `metadata.json` (what Koma-1 will write when you edit a book). A field set
//! in a stronger layer wins; genres and tags are combined, the stronger
//! layer's first. The file name fills what is still missing (see
//! [`crate::names`]).
//!
//! A further format is one more [`Parser`] in [`PARSERS`]. Every parser
//! takes hostile bytes: bad input gives an empty [`Meta`], never an error.

/// Metadata files are read up to this size; a bigger one is ignored.
pub const MAX_META_BYTES: u64 = 1024 * 1024;

/// What a metadata file says about a book. Empty strings and lists mean
/// "not said".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Meta {
    pub title: String,
    pub series: String,
    /// Chapter (or issue) number as written: `3`, `10.5`.
    pub number: String,
    pub volume: String,
    pub summary: String,
    pub authors: Vec<String>,
    pub publishers: Vec<String>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub year: Option<i32>,
    pub language: String,
    pub direction: Option<Direction>,
    /// `release` / `Format`: digital, print…
    pub format: String,
    /// The page to use as cover, 0-based.
    pub cover_page: Option<u32>,
    /// This book's cover stands for its series.
    pub series_cover: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    LeftToRight,
    RightToLeft,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::LeftToRight => "ltr",
            Direction::RightToLeft => "rtl",
        }
    }
}

/// A metadata format.
pub struct Parser {
    /// The file name it reads, compared without case, in any folder of the
    /// archive (or in a photo folder).
    pub file_name: &'static str,
    pub parse: fn(&[u8]) -> Meta,
}

/// Every format, weakest first.
pub const PARSERS: [Parser; 2] = [
    Parser {
        file_name: "comicinfo.xml",
        parse: comic_info,
    },
    Parser {
        file_name: "metadata.json",
        parse: metadata_json,
    },
];

/// The layer a file belongs to, if it is a metadata file: its index in
/// [`PARSERS`].
pub fn layer_of(path: &str) -> Option<usize> {
    let name = path.rsplit('/').next().unwrap_or(path);
    PARSERS
        .iter()
        .position(|p| p.file_name.eq_ignore_ascii_case(name))
}

/// Reads and merges the metadata files found, given as `(layer, bytes)`.
pub fn read(files: &[(usize, Vec<u8>)]) -> Meta {
    let mut layers: Vec<&(usize, Vec<u8>)> = files.iter().collect();
    layers.sort_by_key(|(layer, _)| *layer);
    layers
        .into_iter()
        .filter_map(|(layer, bytes)| PARSERS.get(*layer).map(|p| (p.parse)(bytes)))
        .fold(Meta::default(), merge)
}

/// `strong` wins field by field; genres and tags are combined.
pub fn merge(weak: Meta, strong: Meta) -> Meta {
    let pick = |s: String, w: String| if s.is_empty() { w } else { s };
    let pick_list = |s: Vec<String>, w: Vec<String>| if s.is_empty() { w } else { s };
    Meta {
        title: pick(strong.title, weak.title),
        series: pick(strong.series, weak.series),
        number: pick(strong.number, weak.number),
        volume: pick(strong.volume, weak.volume),
        summary: pick(strong.summary, weak.summary),
        authors: pick_list(strong.authors, weak.authors),
        publishers: pick_list(strong.publishers, weak.publishers),
        genres: combine(strong.genres, weak.genres),
        tags: combine(strong.tags, weak.tags),
        year: strong.year.or(weak.year),
        language: pick(strong.language, weak.language),
        direction: strong.direction.or(weak.direction),
        format: pick(strong.format, weak.format),
        cover_page: strong.cover_page.or(weak.cover_page),
        series_cover: strong.series_cover || weak.series_cover,
    }
}

/// `first` then what `then` adds, without repeats (ignoring case).
fn combine(first: Vec<String>, then: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in first.into_iter().chain(then) {
        if !out.iter().any(|o| o.to_lowercase() == t.to_lowercase()) {
            out.push(t);
        }
    }
    out
}

/// `a, b; c` → `[a, b, c]`.
fn split_list(s: &str) -> Vec<String> {
    s.split([',', ';'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_owned)
        .collect()
}

fn year_of(s: &str) -> Option<i32> {
    let d = s.trim().get(..4)?;
    let y: i32 = d.parse().ok()?;
    (d.bytes().all(|b| b.is_ascii_digit()) && (1000..=9999).contains(&y)).then_some(y)
}

/// `ComicInfo.xml`: Title, Series, Number, Volume, Summary, Writer,
/// Publisher, Genre, Tags, Year, LanguageISO, Format, Manga.
pub fn comic_info(bytes: &[u8]) -> Meta {
    let Some(fields) = xml_fields(bytes) else {
        return Meta::default();
    };
    let get = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_owned())
            .unwrap_or_default()
    };
    let manga = get("Manga").to_lowercase();
    Meta {
        title: get("Title"),
        series: get("Series"),
        number: get("Number"),
        volume: get("Volume"),
        summary: get("Summary"),
        authors: split_list(&get("Writer")),
        publishers: split_list(&get("Publisher")),
        genres: split_list(&get("Genre")),
        tags: split_list(&get("Tags")),
        year: year_of(&get("Year")),
        language: get("LanguageISO"),
        direction: (manga == "yesandrighttoleft").then_some(Direction::RightToLeft),
        format: get("Format"),
        ..Meta::default()
    }
}

/// `metadata.json` (gallery-downloader style): title, series (else the
/// title names the series), number, type and genres, summary, release,
/// direction, artists, groups, tags (strings, or `{tag, female, male}`
/// giving `tag ♀` / `tag ♂`), parodys (added to tags), year (else the first
/// four digits of date), language, thumbnail, seriesCover.
pub fn metadata_json(bytes: &[u8]) -> Meta {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let Ok(serde_json::Value::Object(d)) = serde_json::from_slice::<serde_json::Value>(bytes)
    else {
        return Meta::default();
    };
    let text = |k: &str| match d.get(k) {
        Some(serde_json::Value::String(s)) => s.trim().to_owned(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let names = |k: &str| -> Vec<String> {
        match d.get(k) {
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s.trim().to_owned()),
                    serde_json::Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
                .filter(|s| !s.is_empty())
                .collect(),
            _ => Vec::new(),
        }
    };
    let mut tags = Vec::new();
    if let Some(serde_json::Value::Array(list)) = d.get("tags") {
        for t in list {
            match t {
                serde_json::Value::String(s) if !s.trim().is_empty() => {
                    tags.push(s.trim().to_owned())
                }
                serde_json::Value::Object(o) => {
                    let Some(name) = o.get("tag").and_then(|v| v.as_str()).map(str::trim) else {
                        continue;
                    };
                    if name.is_empty() {
                        continue;
                    }
                    let flag = |k: &str| match o.get(k) {
                        Some(serde_json::Value::Bool(b)) => *b,
                        Some(serde_json::Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
                        _ => false,
                    };
                    let (f, m) = (flag("female"), flag("male"));
                    if f {
                        tags.push(format!("{name} ♀"));
                    }
                    if m {
                        tags.push(format!("{name} ♂"));
                    }
                    if !f && !m {
                        tags.push(name.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
    tags.extend(names("parodys"));
    let mut genres = names("genres");
    if genres.is_empty() && !text("type").is_empty() {
        genres.push(text("type"));
    }
    let title = text("title");
    let series = match text("series") {
        s if s.is_empty() => title.clone(),
        s => s,
    };
    let direction = match text("direction").to_lowercase().as_str() {
        "rtl" => Some(Direction::RightToLeft),
        "ltr" => Some(Direction::LeftToRight),
        _ => None,
    };
    Meta {
        title,
        series,
        number: text("number"),
        summary: text("summary"),
        authors: names("artists"),
        publishers: names("groups"),
        genres,
        tags,
        year: year_of(&text("year")).or_else(|| year_of(&text("date"))),
        language: text("language"),
        direction,
        format: text("release"),
        cover_page: d
            .get("thumbnail")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        series_cover: d.get("seriesCover") == Some(&serde_json::Value::Bool(true)),
        ..Meta::default()
    }
}

/// The child elements of an XML document's root that hold only text, as
/// `(name, text)`. A tiny reader for flat documents like `ComicInfo.xml`:
/// declarations, comments, CDATA and the five entities plus numeric ones are
/// understood; anything malformed gives `None`.
fn xml_fields(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let s = std::str::from_utf8(bytes).ok()?;
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut open: Option<(String, String)> = None; // a depth-2 element being read
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        if let Some(r) = rest.strip_prefix("<!--") {
            i += 4 + r.find("-->")? + 3;
        } else if let Some(r) = rest.strip_prefix("<![CDATA[") {
            let end = r.find("]]>")?;
            if let Some((_, text)) = open.as_mut() {
                text.push_str(&r[..end]);
            }
            i += 9 + end + 3;
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            i += rest.find('>')? + 1;
        } else if let Some(r) = rest.strip_prefix("</") {
            let end = r.find('>')?;
            depth = depth.checked_sub(1)?;
            if depth == 1
                && let Some((name, text)) = open.take()
            {
                if name != r[..end].trim() {
                    return None;
                }
                out.push((name, text));
            }
            i += 2 + end + 1;
        } else if let Some(r) = rest.strip_prefix('<') {
            let end = r.find('>')?;
            let tag = &r[..end];
            let self_closing = tag.ends_with('/');
            let name = tag
                .trim_end_matches('/')
                .split_whitespace()
                .next()?
                .to_owned();
            if !self_closing {
                depth += 1;
                if depth > 64 {
                    return None;
                }
                // Only leaf text of the root's children; deeper elements
                // (lists of pages) are skipped.
                open = (depth == 2).then(|| (name, String::new()));
            }
            i += 1 + end + 1;
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            if let Some((_, text)) = open.as_mut() {
                text.push_str(&unescape(&rest[..end])?);
            }
            i += end;
        }
    }
    (depth == 0).then_some(out)
}

fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let r = &rest[amp + 1..];
        let semi = r.find(';').filter(|&n| n <= 10)?;
        let entity = &r[..semi];
        let c = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code =
                    if let Some(hex) = entity.strip_prefix("#x").or(entity.strip_prefix("#X")) {
                        u32::from_str_radix(hex, 16).ok()?
                    } else {
                        entity.strip_prefix('#')?.parse().ok()?
                    };
                char::from_u32(code)?
            }
        };
        out.push(c);
        rest = &r[semi + 1..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMIC_INFO: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!-- made-up book -->
<ComicInfo xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <Title>The First Light</Title>
  <Series>Starfall</Series>
  <Number>3</Number>
  <Summary><![CDATA[Stars <fall> & rise]]></Summary>
  <Writer>Mirelle, Ada Quill</Writer>
  <Publisher>Moonforge &amp; Co</Publisher>
  <Genre>Fantasy, Drama</Genre>
  <Tags>space; sky</Tags>
  <Year>2021</Year>
  <LanguageISO>en</LanguageISO>
  <Manga>YesAndRightToLeft</Manga>
  <Pages><Page Image="0" Type="FrontCover"/></Pages>
  <Empty/>
</ComicInfo>"#;

    #[test]
    fn comic_info_fields() {
        let m = comic_info(COMIC_INFO.as_bytes());
        assert_eq!(m.title, "The First Light");
        assert_eq!(m.series, "Starfall");
        assert_eq!(m.number, "3");
        assert_eq!(m.summary, "Stars <fall> & rise");
        assert_eq!(m.authors, ["Mirelle", "Ada Quill"]);
        assert_eq!(m.publishers, ["Moonforge & Co"]);
        assert_eq!(m.genres, ["Fantasy", "Drama"]);
        assert_eq!(m.tags, ["space", "sky"]);
        assert_eq!(m.year, Some(2021));
        assert_eq!(m.direction, Some(Direction::RightToLeft));
    }

    #[test]
    fn metadata_json_fields() {
        let m = metadata_json(
            br#"{"title": "Starfall Bunny", "artists": ["Mirelle"], "groups": ["Lumen Studio"],
                 "type": "Cosplay", "tags": ["outdoor", {"tag": "glasses", "female": 1, "male": 1},
                 {"tag": "bunny", "female": true}, {"tag": "night"}], "parodys": ["Starfall"],
                 "date": "2023-05-01", "direction": "LTR", "thumbnail": 2, "seriesCover": true,
                 "number": 4}"#,
        );
        assert_eq!(m.series, "Starfall Bunny", "the title names the series");
        assert_eq!(m.authors, ["Mirelle"]);
        assert_eq!(m.publishers, ["Lumen Studio"]);
        assert_eq!(m.genres, ["Cosplay"]);
        assert_eq!(
            m.tags,
            [
                "outdoor",
                "glasses ♀",
                "glasses ♂",
                "bunny ♀",
                "night",
                "Starfall"
            ]
        );
        assert_eq!(m.year, Some(2023));
        assert_eq!(m.direction, Some(Direction::LeftToRight));
        assert_eq!((m.cover_page, m.series_cover), (Some(2), true));
        assert_eq!(m.number, "4");
    }

    #[test]
    fn metadata_json_wins_and_tags_combine() {
        let m = read(&[
            (
                1,
                br#"{"title": "Strong", "tags": ["a", "B"], "genres": ["Drama"]}"#.to_vec(),
            ),
            (0, COMIC_INFO.as_bytes().to_vec()),
        ]);
        assert_eq!(m.title, "Strong");
        assert_eq!(m.series, "Strong", "metadata.json's title names the series");
        assert_eq!(
            m.authors,
            ["Mirelle", "Ada Quill"],
            "gaps filled from ComicInfo.xml"
        );
        assert_eq!(m.tags, ["a", "B", "space", "sky"]);
        assert_eq!(m.genres, ["Drama", "Fantasy"]);
    }

    #[test]
    fn layers_by_name() {
        assert_eq!(layer_of("ComicInfo.xml"), Some(0));
        assert_eq!(layer_of("extras/METADATA.JSON"), Some(1));
        assert_eq!(layer_of("cover.jpg"), None);
    }

    #[test]
    fn garbage_gives_nothing() {
        for bad in [
            &b""[..],
            b"<",
            b"<a><b>x</a>",
            b"\xff\xfe",
            b"<a>&bogus;</a>",
            b"[1,2]",
            b"{",
        ] {
            assert_eq!(comic_info(bad), Meta::default());
            assert_eq!(metadata_json(bad), Meta::default());
        }
        let deep = "<a>".repeat(100) + &"</a>".repeat(100);
        assert_eq!(comic_info(deep.as_bytes()), Meta::default());
    }
}
