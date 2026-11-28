//! The naming standard (docs/naming.md, "Naming standard"): the name a book
//! gets, the rename plan of a series or a library, the fix-up queue of what
//! cannot be named safely, and the tidy that keeps a cosplay library in
//! `Cosplayer / Set / variant` folders.
//!
//! A manga or comics book is named
//! `(Event) [Circle (Author)] Series - Ch. 003 [EN].cbz`, each part only
//! when known, numbers padded to the series' largest. The name reads back
//! (through [`crate::names::book_name`]) as the series, kind, number, author
//! and circle it was made from, so a rename loses nothing.
//!
//! Plans only look at the catalogue and the disk; the worker carries them
//! out as ordinary journaled changes ([`crate::organise`]).

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::catalog::{Catalog, Error};
use crate::libraries::{Kind as LibraryKind, Library};
use crate::names::{self, Kind};

/// What goes into a book's name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Parts {
    pub event: Option<String>,
    pub circle: Option<String>,
    pub author: Option<String>,
    pub series: String,
    pub kind: Option<Kind>,
    pub number: Option<f64>,
    /// Two letters, upper case: `EN`.
    pub language: Option<String>,
}

/// The event in front of a name: `(SF12) [..] Series` → `SF12`.
pub fn event_of(stem: &str) -> Option<String> {
    let rest = stem.trim_start().strip_prefix('(')?;
    let (event, _) = rest.split_once(')')?;
    let event = event.trim();
    (!event.is_empty() && !event.contains(['(', '[', ']'])).then(|| event.to_owned())
}

/// The language a word or code names, as two upper-case letters.
pub fn language_code(word: &str) -> Option<String> {
    let w = word.trim().to_lowercase();
    let code = match w.as_str() {
        "en" | "eng" | "english" => "EN",
        "ja" | "jp" | "jpn" | "japanese" => "JA",
        "zh" | "chi" | "chinese" => "ZH",
        "ko" | "kor" | "korean" => "KO",
        "fr" | "fre" | "fra" | "french" => "FR",
        "de" | "ger" | "deu" | "german" => "DE",
        "es" | "spa" | "spanish" => "ES",
        "it" | "ita" | "italian" => "IT",
        "pt" | "por" | "portuguese" => "PT",
        "ru" | "rus" | "russian" => "RU",
        _ => return None,
    };
    Some(code.to_owned())
}

/// The language in a name's last brackets: `… [English]` → `EN`.
pub fn language_of(stem: &str) -> Option<String> {
    let rest = stem.trim_end().strip_suffix(']')?;
    let (_, word) = rest.rsplit_once('[')?;
    language_code(word)
}

/// Digits of the integer part of `n`.
fn digits(n: f64) -> usize {
    (n.trunc().max(0.0) as u64).to_string().len()
}

/// `n` with its integer part padded to `width`: `3` → `003`, `10.5` → `010.5`.
pub fn padded(n: f64, width: usize) -> String {
    let int = n.trunc().max(0.0) as u64;
    let frac = n - n.trunc();
    let mut s = format!("{int:0width$}");
    if frac > 1e-9 {
        let f = format!("{frac:.4}");
        s.push_str(f.trim_start_matches('0').trim_end_matches('0'));
    }
    s
}

/// The widths numbers are padded to in a series: the digits of its largest
/// chapter, and of its largest volume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Widths {
    pub chapter: usize,
    pub volume: usize,
}

impl Widths {
    pub fn of(numbers: impl IntoIterator<Item = (Option<Kind>, f64)>) -> Self {
        let mut w = Widths {
            chapter: 1,
            volume: 1,
        };
        for (kind, n) in numbers {
            match kind {
                Some(Kind::Volume) => w.volume = w.volume.max(digits(n)),
                _ => w.chapter = w.chapter.max(digits(n)),
            }
        }
        w
    }
}

/// A book's name, without its extension.
pub fn build(p: &Parts, widths: Widths) -> String {
    let mut s = String::new();
    if let Some(e) = &p.event {
        s.push_str(&format!("({e}) "));
    }
    match (&p.circle, &p.author) {
        (Some(c), Some(a)) => s.push_str(&format!("[{c} ({a})] ")),
        (None, Some(a)) => s.push_str(&format!("[{a}] - ")),
        (Some(c), None) => s.push_str(&format!("[{c}] ")),
        (None, None) => {}
    }
    s.push_str(&p.series);
    if let Some(n) = p.number {
        s.push_str(" - ");
        match p.kind {
            Some(Kind::Volume) => s.push_str(&format!("Vol. {}", padded(n, widths.volume))),
            Some(Kind::Chapter) => s.push_str(&format!("Ch. {}", padded(n, widths.chapter))),
            None => s.push_str(&padded(n, widths.chapter)),
        }
    }
    if let Some(l) = &p.language {
        s.push_str(&format!(" [{l}]"));
    }
    s
}

/// A name with characters no file name may hold replaced.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a plan does with one book.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Its name is right already.
    Same,
    /// Renamed to `to`.
    Rename,
    /// Not named safely: why, for the fix-up queue.
    Fixup(String),
}

/// One book in a rename plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub book: i64,
    pub series: i64,
    /// Path now, relative to the library.
    pub from: String,
    /// Path it gets (or would get), relative to the library.
    pub to: String,
    pub status: Status,
    /// What the name is made of (shown in the fix-up queue).
    pub parts: Parts,
    pub photos: bool,
    /// A set in a cosplay library (the tidy), not a manga or comics book.
    pub cosplay: bool,
}

/// The most common value of a list, if any.
fn most_common(values: impl IntoIterator<Item = String>) -> Option<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut order = Vec::new();
    for v in values {
        if !counts.contains_key(&v) {
            order.push(v.clone());
        }
        *counts.entry(v).or_default() += 1;
    }
    order.into_iter().max_by_key(|v| counts[v])
}

/// The rename plan of one series: every present book, renamed, already
/// right, or for the fix-up queue. `kept`: paths whose name is to be kept.
pub fn series_rows(c: &Catalog, series: i64, kept: &HashSet<String>) -> Result<Vec<Row>, Error> {
    let Some(s) = c.series_by_id(series)? else {
        return Ok(Vec::new());
    };
    let books = c.books(series)?;
    let mut info = Vec::new();
    for b in &books {
        let d = c.details(&b.path)?.unwrap_or_default();
        let file = b.path.rsplit('/').next().unwrap_or(&b.path).to_owned();
        let (stem, ext) = match (b.photos, file.rsplit_once('.')) {
            (false, Some((stem, ext))) => (stem.to_owned(), Some(ext.to_owned())),
            _ => (file.clone(), None),
        };
        info.push((b, d, stem, ext));
    }
    let author = most_common(info.iter().filter_map(|i| i.1.authors.first().cloned()));
    let circle = most_common(info.iter().filter_map(|i| i.1.publishers.first().cloned()));
    let language = most_common(
        info.iter()
            .filter_map(|i| language_code(&i.1.language).or_else(|| language_of(&i.2))),
    );
    let kind_of = |k: &Option<String>| match k.as_deref() {
        Some("volume") => Some(Kind::Volume),
        Some("chapter") => Some(Kind::Chapter),
        _ => None,
    };
    let widths = Widths::of(
        info.iter()
            .filter_map(|i| Some((kind_of(&i.1.kind), i.1.number?))),
    );
    let series_name = clean(&s.name);
    let several = books.len() > 1;
    let mut rows = Vec::new();
    for (b, d, stem, ext) in &info {
        let parts = Parts {
            event: event_of(stem),
            circle: d
                .publishers
                .first()
                .cloned()
                .or_else(|| circle.clone())
                .map(|x| clean(&x)),
            author: d
                .authors
                .first()
                .cloned()
                .or_else(|| author.clone())
                .map(|x| clean(&x)),
            series: series_name.clone(),
            kind: kind_of(&d.kind),
            number: d.number,
            language: language_code(&d.language)
                .or_else(|| language_of(stem))
                .or_else(|| language.clone()),
        };
        let name = build(&parts, widths);
        let file = match ext {
            Some(e) => format!("{name}.{}", e.to_lowercase()),
            None => name,
        };
        let folder = crate::scan::folder_of_path(&b.path);
        let to = if folder.is_empty() {
            file
        } else {
            format!("{folder}/{file}")
        };
        let status = if kept.contains(&b.path) || to == b.path {
            Status::Same
        } else if series_name.is_empty() {
            Status::Fixup("no series name".into())
        } else if parts.number.is_none() && several {
            Status::Fixup("no number, and not the series' only book".into())
        } else {
            Status::Rename
        };
        rows.push(Row {
            book: b.id,
            series,
            from: b.path.clone(),
            to,
            status,
            parts,
            photos: b.photos,
            cosplay: false,
        });
    }
    Ok(rows)
}

/// Sends to the fix-up queue every rename whose new name another book in
/// the plan also gets, or a file not being renamed already has.
pub fn mark_clashes(rows: &mut [Row], library: &Path) {
    let mut count: HashMap<String, usize> = HashMap::new();
    for r in rows.iter() {
        if r.status != Status::Same || r.to == r.from {
            *count.entry(r.to.to_lowercase()).or_default() += 1;
        }
    }
    let staying: HashSet<String> = rows
        .iter()
        .filter(|r| r.status != Status::Rename)
        .map(|r| r.from.to_lowercase())
        .collect();
    let moving: HashSet<String> = rows
        .iter()
        .filter(|r| r.status == Status::Rename)
        .map(|r| r.from.to_lowercase())
        .collect();
    for r in rows.iter_mut() {
        if r.status != Status::Rename {
            continue;
        }
        let key = r.to.to_lowercase();
        let taken_on_disk = std::fs::symlink_metadata(library.join(&r.to)).is_ok()
            && !moving.contains(&key)
            && key != r.from.to_lowercase();
        if count.get(&key).copied().unwrap_or(0) > 1 || staying.contains(&key) || taken_on_disk {
            let file = r.to.rsplit('/').next().unwrap_or(&r.to);
            r.status = Status::Fixup(format!("another file would also be {file}"));
        }
    }
}

/// The rename plan of every series of a manga or comics library.
pub fn library_rows(
    c: &Catalog,
    library: &Path,
    folder: &str,
    kept: &HashSet<String>,
) -> Result<Vec<Row>, Error> {
    let mut rows = Vec::new();
    for (s, _) in c.series_in_library(folder)? {
        rows.extend(series_rows(c, s.id, kept)?);
    }
    mark_clashes(&mut rows, library);
    Ok(rows)
}

/// The library folders that are cosplay libraries (by their settings or
/// their name), from the first folder of every book.
pub fn cosplay_libraries(c: &Catalog) -> Result<Vec<String>, Error> {
    let settings = c.library_settings()?;
    let mut folders: Vec<String> = c
        .book_tops()?
        .into_iter()
        .chain(settings.iter().map(|s| s.folder.clone()))
        .filter(|f| !f.is_empty() && !f.starts_with('.'))
        .collect();
    // In a private folder the library is the second level.
    folders.sort();
    folders.dedup();
    Ok(folders
        .into_iter()
        .filter(|f| {
            let s = settings.iter().find(|s| &s.folder == f);
            Library::from_folder(f, 0, 0).with(s).kind == LibraryKind::Cosplay
        })
        .collect())
}

/// The tidy of a cosplay library: every set moved into
/// `<library>/<Cosplayer>/<Set>/`, keeping its own name. Sets with no
/// cosplayer in their name are left, for the fix-up queue.
pub fn tidy_rows(
    c: &Catalog,
    library: &Path,
    folder: &str,
    kept: &HashSet<String>,
) -> Result<Vec<Row>, Error> {
    let mut rows = Vec::new();
    for (id, path, photos, series) in c.books_under(folder)? {
        let rest = &path[folder.len() + 1..];
        let file = rest.rsplit('/').next().unwrap_or(rest);
        let stem = if photos {
            file
        } else {
            file.rsplit_once('.').map_or(file, |(s, _)| s)
        };
        let n = names::cosplay_name(stem);
        let (who, set) = (clean(&n.cosplayer), clean(&n.set));
        let parts = Parts {
            circle: (!n.circle.is_empty()).then(|| n.circle.clone()),
            author: (!who.is_empty()).then(|| who.clone()),
            series: set.clone(),
            ..Parts::default()
        };
        let home = format!("{folder}/{who}/{set}");
        let to = format!("{home}/{file}");
        let status = if kept.contains(&path) || path.starts_with(&format!("{home}/")) {
            Status::Same
        } else if who.is_empty() || set.is_empty() {
            Status::Fixup("no cosplayer in the name".into())
        } else if !crate::organise::safe_name(&who) || !crate::organise::safe_name(&set) {
            Status::Fixup("the name can't be a folder name".into())
        } else {
            Status::Rename
        };
        rows.push(Row {
            book: id,
            series,
            from: path,
            to,
            status,
            parts,
            photos,
            cosplay: true,
        });
    }
    mark_clashes(&mut rows, library);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_and_languages() {
        assert_eq!(
            event_of("(SF12) [Moonforge (Mirelle)] Starfall").as_deref(),
            Some("SF12")
        );
        assert_eq!(event_of("[Moonforge] Starfall"), None);
        assert_eq!(
            language_of("Starfall - Ch. 3 [English]").as_deref(),
            Some("EN")
        );
        assert_eq!(language_of("starfall_c3_[eng]").as_deref(), Some("EN"));
        assert_eq!(language_of("Starfall [Digital]"), None);
    }

    #[test]
    fn padding() {
        assert_eq!(padded(3.0, 3), "003");
        assert_eq!(padded(10.5, 3), "010.5");
        assert_eq!(padded(7.25, 1), "7.25");
        assert_eq!(
            Widths::of([(Some(Kind::Chapter), 120.0), (Some(Kind::Volume), 2.0)]),
            Widths {
                chapter: 3,
                volume: 1
            }
        );
    }

    #[test]
    fn names_read_back_as_what_made_them() {
        let w = Widths {
            chapter: 3,
            volume: 2,
        };
        let cases = [
            Parts {
                event: Some("SF12".into()),
                circle: Some("Moonforge".into()),
                author: Some("Mirelle".into()),
                series: "Starfall Academy".into(),
                kind: Some(Kind::Chapter),
                number: Some(3.0),
                language: Some("EN".into()),
            },
            Parts {
                author: Some("Mirelle".into()),
                series: "Starfall Academy".into(),
                kind: Some(Kind::Volume),
                number: Some(2.0),
                ..Parts::default()
            },
            Parts {
                series: "Moonforge Chronicle".into(),
                kind: Some(Kind::Chapter),
                number: Some(10.5),
                ..Parts::default()
            },
            Parts {
                circle: Some("Moonforge".into()),
                author: Some("Mirelle".into()),
                series: "Lone Comet".into(),
                ..Parts::default()
            },
        ];
        for p in cases {
            let name = build(&p, w);
            let back = names::book_name(&name);
            assert_eq!(back.series.as_deref(), Some(p.series.as_str()), "{name}");
            assert_eq!((back.kind, back.number), (p.kind, p.number), "{name}");
            assert_eq!(back.author, p.author, "{name}");
            assert_eq!(back.publisher, p.circle, "{name}");
            assert_eq!(event_of(&name), p.event, "{name}");
            assert_eq!(language_of(&name), p.language, "{name}");
        }
        assert_eq!(
            build(
                &Parts {
                    event: Some("SF12".into()),
                    circle: Some("Moonforge".into()),
                    author: Some("Mirelle".into()),
                    series: "Starfall".into(),
                    kind: Some(Kind::Chapter),
                    number: Some(3.0),
                    language: Some("EN".into()),
                },
                w
            ),
            "(SF12) [Moonforge (Mirelle)] Starfall - Ch. 003 [EN]"
        );
    }

    #[test]
    fn file_names_are_cleaned() {
        assert_eq!(clean("Star/fall: Zero?"), "Star fall Zero");
    }
}
