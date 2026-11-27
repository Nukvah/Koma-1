//! Libraries: the top-level folders of `/library` that hold series.
//!
//! A library is not stored: it is the first folder of its series' paths.
//! `manga/Starfall` is the series Starfall in the library `manga`. A series
//! with a one-part path (`Starfall`, a photo folder, or books loose in the
//! root grouped by name) is in the default library, `""`, named "Library".
//! So a top-level folder that holds books directly is a series, and one that
//! holds series folders is a library, with no rule to keep in step.
//!
//! Inside a library, a folder that holds series folders is a collection:
//! `manga/Starfall Saga/Starfall` is in the collection `Starfall Saga`.
//!
//! Until accounts exist (M5) a library's settings are read-only and come
//! from its folder name: see [`Library::from_folder`].

/// What a library holds: sets how file names are read and the default
/// reading direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Manga,
    Comics,
    Cosplay,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Manga => "manga",
            Kind::Comics => "comics",
            Kind::Cosplay => "cosplay",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    RightToLeft,
    LeftToRight,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::RightToLeft => "right to left",
            Direction::LeftToRight => "left to right",
        }
    }
}

/// Library colours, given out in library order.
pub const COLOURS: [(&str, &str); 6] = [
    ("amber", "#F2A33A"),
    ("teal", "#4FB3A9"),
    ("rose", "#D9708A"),
    ("slate", "#8A9BB0"),
    ("violet", "#9C7FD9"),
    ("green", "#7FC79A"),
];

/// The name of the default library, for series not in a library folder.
pub const DEFAULT_NAME: &str = "Library";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Library {
    /// Its folder under `/library`; `""` for the default library.
    pub folder: String,
    pub name: String,
    pub kind: Kind,
    pub direction: Direction,
    /// `(name, CSS colour)` from [`COLOURS`].
    pub colour: (&'static str, &'static str),
    /// Series with a present book.
    pub series: i64,
}

impl Library {
    /// A library's settings from its folder name, at position `index` in
    /// library order:
    /// - name: the folder's name with its first letter capitalised;
    /// - kind: comics if the name says "comic", cosplay if it says
    ///   "cosplay", else manga;
    /// - direction: right to left for manga, left to right otherwise;
    /// - colour: the `index`th of [`COLOURS`], round and round.
    pub fn from_folder(folder: &str, index: usize, series: i64) -> Self {
        let name = if folder.is_empty() {
            DEFAULT_NAME.to_owned()
        } else {
            let mut chars = folder.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        };
        let lower = folder.to_lowercase();
        let kind = if lower.contains("comic") {
            Kind::Comics
        } else if lower.contains("cosplay") {
            Kind::Cosplay
        } else {
            Kind::Manga
        };
        let direction = match kind {
            Kind::Manga => Direction::RightToLeft,
            Kind::Comics | Kind::Cosplay => Direction::LeftToRight,
        };
        Self {
            folder: folder.to_owned(),
            name,
            kind,
            direction,
            colour: COLOURS[index % COLOURS.len()],
            series,
        }
    }

    /// Every library, from [`crate::catalog::Catalog::library_keys`] (the
    /// default first, then folders in natural order).
    pub fn all(keys: Vec<(String, i64)>) -> Vec<Library> {
        keys.into_iter()
            .enumerate()
            .map(|(i, (folder, series))| Library::from_folder(&folder, i, series))
            .collect()
    }

    /// Its page: `/` for the default library, `/l/<folder>` for the others.
    pub fn url(&self) -> String {
        if self.folder.is_empty() {
            "/".to_owned()
        } else {
            format!("/l/{}", encode_path_part(&self.folder))
        }
    }
}

/// The collection a series is in, from its path: the folder above the series
/// folder when that is not the library folder itself.
/// `manga/Starfall Saga/Starfall` → `Starfall Saga`; `manga/Starfall` → none.
pub fn collection_of(series_path: &str) -> Option<&str> {
    if series_path.starts_with('\0') {
        return None;
    }
    let (parent, _) = series_path.rsplit_once('/')?;
    let (_, collection) = parent.split_once('/')?;
    Some(collection)
}

/// Percent-encodes one path segment for a URL: everything but unreserved
/// ASCII (RFC 3986), so any folder name round-trips.
pub fn encode_path_part(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_come_from_the_folder_name() {
        let l = Library::from_folder("manga", 0, 3);
        assert_eq!(
            (l.name.as_str(), l.kind, l.direction, l.colour.0),
            ("Manga", Kind::Manga, Direction::RightToLeft, "amber")
        );
        let l = Library::from_folder("My Comics", 1, 3);
        assert_eq!(
            (l.name.as_str(), l.kind, l.direction, l.colour.0),
            ("My Comics", Kind::Comics, Direction::LeftToRight, "teal")
        );
        let l = Library::from_folder("cosplay sets", 2, 3);
        assert_eq!(
            (l.kind, l.direction),
            (Kind::Cosplay, Direction::LeftToRight)
        );
        // Webtoons and anything else are read as manga.
        assert_eq!(Library::from_folder("webtoons", 3, 1).kind, Kind::Manga);
        assert_eq!(Library::from_folder("ébauches", 3, 1).name, "Ébauches");
        assert_eq!(Library::from_folder("", 0, 1).name, "Library");
        // Colours go round.
        assert_eq!(Library::from_folder("x", 6, 1).colour.0, "amber");
    }

    #[test]
    fn collections() {
        assert_eq!(
            collection_of("manga/Starfall Saga/Starfall"),
            Some("Starfall Saga")
        );
        assert_eq!(collection_of("manga/a/b/Starfall"), Some("a/b"));
        assert_eq!(collection_of("manga/Starfall"), None);
        assert_eq!(collection_of("Starfall"), None);
        assert_eq!(collection_of("\0starfall"), None);
    }

    #[test]
    fn urls() {
        assert_eq!(Library::from_folder("", 0, 1).url(), "/");
        assert_eq!(Library::from_folder("manga", 1, 1).url(), "/l/manga");
        assert_eq!(
            Library::from_folder("Mirelle's 漫画", 1, 1).url(),
            "/l/Mirelle%27s%20%E6%BC%AB%E7%94%BB"
        );
    }
}
