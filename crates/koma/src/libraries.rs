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
//! A top-level folder named `~name` belongs to the account `name`: the
//! libraries in it (`~mirelle/sketchbooks`) are seen by that account alone.
//! There the library is the first two folders; a series straight in `~name`
//! is in the library `~name`, so nothing in it ever falls into a shared one.
//!
//! A library's settings come from its folder name ([`Library::from_folder`])
//! until they are changed in the UI; changes are stored in the catalogue
//! ([`Settings`]) by the worker.

/// What a library holds: sets how file names are read and the default
/// reading direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Manga,
    Comics,
    Cosplay,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Manga, Kind::Comics, Kind::Cosplay];

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

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
    /// `rtl` or `ltr`, as stored.
    pub fn code(self) -> &'static str {
        match self {
            Direction::RightToLeft => "rtl",
            Direction::LeftToRight => "ltr",
        }
    }

    pub fn from_code(s: &str) -> Option<Direction> {
        match s {
            "rtl" => Some(Direction::RightToLeft),
            "ltr" => Some(Direction::LeftToRight),
            _ => None,
        }
    }

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

/// A colour of [`COLOURS`] by its name.
pub fn colour(name: &str) -> Option<(&'static str, &'static str)> {
    COLOURS.into_iter().find(|c| c.0 == name)
}

/// A library's settings as changed in the UI, stored in the catalogue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub folder: String,
    pub name: String,
    pub kind: Kind,
    pub direction: Direction,
    pub colour: (&'static str, &'static str),
}

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
        let last = folder.rsplit('/').next().unwrap_or(folder);
        let name = if folder.is_empty() {
            DEFAULT_NAME.to_owned()
        } else {
            let mut chars = last.chars();
            chars
                .next()
                .map(|c| c.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        };
        let lower = last.to_lowercase();
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

    /// Its settings as changed in the UI, over those from its folder.
    pub fn with(mut self, settings: Option<&Settings>) -> Self {
        if let Some(s) = settings {
            self.name.clone_from(&s.name);
            self.kind = s.kind;
            self.direction = s.direction;
            self.colour = s.colour;
        }
        self
    }

    /// Every library, from [`crate::catalog::Catalog::library_keys`] (the
    /// default first, then folders in natural order), with the settings
    /// changed in the UI.
    pub fn all(keys: Vec<(String, i64)>, settings: &[Settings]) -> Vec<Library> {
        keys.into_iter()
            .enumerate()
            .map(|(i, (folder, series))| {
                let changed = settings.iter().find(|s| s.folder == folder);
                Library::from_folder(&folder, i, series).with(changed)
            })
            .collect()
    }

    /// The account whose folder it is in (`~name/…`), if any.
    pub fn owner(&self) -> Option<&str> {
        owner_of(&self.folder)
    }

    /// Whether the account `name` may see it: shared libraries everyone,
    /// one in `~name` that account alone.
    pub fn visible_to(&self, name: &str) -> bool {
        self.owner().is_none_or(|o| o == name)
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

/// The library folder a series is in, from its path, as the catalogue's
/// `series.library` column has it: `""` for the default library. In an
/// account's folder, the first two folders (`~mirelle/sketchbooks`), or
/// `~mirelle` itself for a series straight in it.
pub fn library_of(series_path: &str) -> &str {
    if series_path.starts_with('\0') {
        return "";
    }
    if series_path.starts_with('~') {
        let mut slashes = series_path.match_indices('/').map(|(i, _)| i);
        return match (slashes.next(), slashes.next()) {
            (None, _) => series_path,
            (Some(i), None) => &series_path[..i],
            (Some(_), Some(j)) => &series_path[..j],
        };
    }
    series_path
        .split_once('/')
        .map_or("", |(library, _)| library)
}

/// The collection a series is in, from its path: the folders between its
/// library's and its own, if any.
/// `manga/Starfall Saga/Starfall` → `Starfall Saga`; `manga/Starfall` → none.
pub fn collection_of(series_path: &str) -> Option<&str> {
    let library = library_of(series_path);
    if library.is_empty() || library.len() >= series_path.len() {
        return None;
    }
    let rest = &series_path[library.len() + 1..];
    rest.rsplit_once('/').map(|(collection, _)| collection)
}

/// The account a library folder belongs to: `~mirelle/…` → `mirelle`.
pub fn owner_of(folder: &str) -> Option<&str> {
    let rest = folder.strip_prefix('~')?;
    Some(rest.split_once('/').map_or(rest, |(name, _)| name))
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
    fn library_of_a_series() {
        assert_eq!(library_of("manga/Starfall Saga/Starfall"), "manga");
        assert_eq!(library_of("manga/Starfall"), "manga");
        assert_eq!(library_of("Starfall"), "");
        assert_eq!(library_of("\0starfall"), "");
    }

    #[test]
    fn private_libraries() {
        assert_eq!(
            library_of("~mirelle/sketchbooks/Starfall"),
            "~mirelle/sketchbooks"
        );
        assert_eq!(
            library_of("~mirelle/sketchbooks/a/Starfall"),
            "~mirelle/sketchbooks"
        );
        assert_eq!(library_of("~mirelle/Starfall"), "~mirelle");
        assert_eq!(library_of("~mirelle"), "~mirelle");
        assert_eq!(collection_of("~mirelle/sketchbooks/a/Starfall"), Some("a"));
        assert_eq!(collection_of("~mirelle/sketchbooks/Starfall"), None);
        assert_eq!(collection_of("~mirelle/Starfall"), None);
        assert_eq!(collection_of("~mirelle"), None);
        assert_eq!(owner_of("~mirelle/sketchbooks"), Some("mirelle"));
        assert_eq!(owner_of("~mirelle"), Some("mirelle"));
        assert_eq!(owner_of("~"), Some(""));
        assert_eq!(owner_of("manga"), None);
        let l = Library::from_folder("~mirelle/sketch comics", 2, 1);
        assert_eq!((l.name.as_str(), l.kind), ("Sketch comics", Kind::Comics));
        assert!(l.visible_to("mirelle"));
        assert!(!l.visible_to("bram"));
        assert!(Library::from_folder("manga", 1, 1).visible_to("bram"));
        assert!(!Library::from_folder("~", 1, 1).visible_to("bram"));
        assert_eq!(l.url(), "/l/~mirelle%2Fsketch%20comics");
    }

    #[test]
    fn changed_settings() {
        let s = Settings {
            folder: "webtoons".into(),
            name: "Webtoon".into(),
            kind: Kind::Comics,
            direction: Direction::LeftToRight,
            colour: colour("rose").unwrap(),
        };
        let all = Library::all(vec![("manga".into(), 1), ("webtoons".into(), 2)], &[s]);
        assert_eq!(all[0].name, "Manga");
        assert_eq!(
            (all[1].name.as_str(), all[1].kind, all[1].colour.0),
            ("Webtoon", Kind::Comics, "rose")
        );
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
