//! The catalogue: series, books and pages in SQLite.
//!
//! `koma worker` writes it; `koma serve` only reads it. Names and paths sort
//! with the `natsort` collation: [`crate::natsort::compare_paths`].
//!
//! It also holds what the scan needs to stay incremental (folders and their
//! modification times) and the worker's job queue.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

pub use crate::archive::size::Size;

pub use rusqlite::Error;

const SCHEMA_VERSION: i64 = 11;

/// How long a book whose file vanished is remembered, so that a folder that
/// comes back (a drive mounted late, a move noticed in two steps) keeps its
/// books' ids, progress and dates.
pub const GONE_GRACE_SECS: i64 = 30 * 24 * 60 * 60;

/// A job claimed this many times without finishing is given up: the worker
/// died on it each time (crash, out of memory), so it would only die again.
pub const MAX_ATTEMPTS: i64 = 3;

const SCHEMA: &str = "
CREATE TABLE series (
    id    INTEGER PRIMARY KEY AUTOINCREMENT,  -- never reused: ids are kept for good
    path  TEXT NOT NULL UNIQUE,      -- folder relative to the library; for books loose in the
                                     -- root, NUL + the lower-cased series name from the file name
    name  TEXT NOT NULL,
    library TEXT GENERATED ALWAYS AS ({LIBRARY_OF_PATH}) VIRTUAL,
    collection TEXT GENERATED ALWAYS AS ({COLLECTION_OF_PATH}) VIRTUAL
);
CREATE INDEX series_library ON series(library);
CREATE INDEX series_shelf ON series(library, collection, name COLLATE natsort, path COLLATE natsort);
CREATE INDEX series_names ON series(library, name COLLATE natsort, path COLLATE natsort);
CREATE TABLE books (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,  -- never reused: ids are kept for good
    series_id  INTEGER NOT NULL REFERENCES series(id),
    path       TEXT NOT NULL UNIQUE, -- file (or photo folder) relative to the library
    folder     TEXT NOT NULL,        -- its folder relative to the library ('' is the root);
                                     -- a photo folder's own path
    name       TEXT NOT NULL,
    size       INTEGER NOT NULL,     -- with mtime: skip unchanged files on rescan
    mtime      INTEGER NOT NULL,
    page_count INTEGER NOT NULL,
    added      INTEGER NOT NULL,     -- unix seconds: when first seen; kept through moves
    gone       INTEGER,              -- unix seconds: when its file vanished; NULL while present
    -- Details: its metadata files (metadata.json over ComicInfo.xml), gaps
    -- filled from the file name. See docs/naming.md.
    title      TEXT NOT NULL DEFAULT '',
    number     REAL,                 -- chapter or volume number
    kind       TEXT,                 -- 'chapter' or 'volume'
    summary    TEXT NOT NULL DEFAULT '',
    year       INTEGER,
    language   TEXT NOT NULL DEFAULT '',
    direction  TEXT,                 -- 'ltr' or 'rtl'; NULL: not stated
    cover_page INTEGER               -- 0-based page to show as the cover; NULL: the first
);
CREATE INDEX books_series ON books(series_id);
CREATE INDEX books_folder ON books(folder);
CREATE INDEX books_gone_stamp ON books(size, mtime) WHERE gone IS NOT NULL;
CREATE TABLE book_terms (
    book_id  INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    kind     TEXT NOT NULL,          -- 'author', 'publisher', 'genre' or 'tag'
    name     TEXT NOT NULL,
    pos      INTEGER NOT NULL,       -- order as given
    PRIMARY KEY (book_id, kind, pos)
) WITHOUT ROWID;
CREATE INDEX book_terms_name ON book_terms(kind, name);
CREATE TABLE pages (
    book_id  INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    number   INTEGER NOT NULL,       -- 0-based reading order
    hash     BLOB NOT NULL,          -- BLAKE3 of the page bytes
    format   TEXT NOT NULL,          -- file extension: jpg, png, webp...
    length   INTEGER NOT NULL,
    offset   INTEGER,                -- byte offset in the book file; NULL: in the blob store
    file     TEXT,                   -- the file holding the page if not the book's (photo folders)
    width    INTEGER,                -- as displayed (EXIF orientation applied); NULL: unreadable header
    height   INTEGER,
    PRIMARY KEY (book_id, number)
) WITHOUT ROWID;
CREATE INDEX pages_hash ON pages(hash);
-- An EPUB's chapters, cleaned at ingest (crate::epub), and its contents.
-- Its pictures are its pages.
CREATE TABLE chapters (
    book_id  INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    number   INTEGER NOT NULL,       -- 0-based reading order
    hash     BLOB NOT NULL,          -- BLAKE3 of the cleaned chapter: blob <hash>.html
    length   INTEGER NOT NULL,
    title    TEXT NOT NULL,          -- its first entry in the contents, else ''
    PRIMARY KEY (book_id, number)
) WITHOUT ROWID;
CREATE TABLE toc (
    book_id  INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    pos      INTEGER NOT NULL,       -- order in the contents
    level    INTEGER NOT NULL,       -- 1 for the top level
    title    TEXT NOT NULL,
    chapter  INTEGER NOT NULL,       -- 0-based chapter number
    anchor   TEXT,                   -- the place in it (an id, as the book writes it)
    PRIMARY KEY (book_id, pos)
) WITHOUT ROWID;
CREATE TABLE scan_errors (
    path    TEXT PRIMARY KEY,        -- a book file, or a folder that could not be read
    folder  TEXT NOT NULL,           -- the folder it is in
    size    INTEGER,                 -- with mtime: don't retry an unchanged broken file
    mtime   INTEGER,
    error   TEXT NOT NULL
);
CREATE INDEX scan_errors_folder ON scan_errors(folder);
CREATE TABLE folders (
    path    TEXT PRIMARY KEY,        -- relative to the library; '' is the root
    parent  TEXT,                    -- NULL for the root
    mtime   INTEGER                  -- when last read in full; NULL: read it next time
);
CREATE INDEX folders_parent ON folders(parent);
CREATE TABLE jobs (
    id        INTEGER PRIMARY KEY,
    kind      TEXT NOT NULL,         -- see JobKind
    path      TEXT NOT NULL,         -- what it works on, relative to the library
    running   INTEGER NOT NULL DEFAULT 0,
    attempts  INTEGER NOT NULL DEFAULT 0,
    UNIQUE (kind, path)
);
-- Page variants (see crate::variants), by page content: a page in two books,
-- or moved, has them once. Their bytes are blobs named <hash>.<width>.<format>.
CREATE TABLE variants (
    hash    BLOB NOT NULL,           -- the page's
    width   INTEGER NOT NULL,
    format  TEXT NOT NULL,           -- 'jpg' or 'webp'
    height  INTEGER NOT NULL,
    length  INTEGER NOT NULL,
    PRIMARY KEY (hash, width, format)
) WITHOUT ROWID;
-- Search (crate::search): one row per present series (its name and its
-- books' people, genres and tags) and per present book (its title), rebuilt
-- by the worker after a scan that changed something. Trigram: any part of a
-- word matches, so Japanese and Korean need no word splitter.
CREATE VIRTUAL TABLE search USING fts5(
    name, people, genres, tags,
    kind UNINDEXED,                  -- 's' series, 'b' book
    ref UNINDEXED,                   -- its id
    tokenize = 'trigram'
);
CREATE TABLE variant_pages (
    hash     BLOB PRIMARY KEY,       -- a page whose variants are made (maybe none)
    version  INTEGER NOT NULL,       -- variants::VERSION they were made with
    error    TEXT                    -- why it has none, if it could not be decoded
) WITHOUT ROWID;
";

/// The library a series is in, from its path (see `crate::libraries`): the
/// first folder of a path with more than one, else `''`, the default library.
const LIBRARY_OF_PATH: &str = "CASE WHEN substr(path, 1, 1) <> char(0) AND instr(path, '/') > 0 \
     THEN substr(path, 1, instr(path, '/') - 1) ELSE '' END";

/// Schema 7 to 8: series get their library. Nothing else changes, so the
/// catalogue (and the page variants, which are costly to remake) is kept.
fn migrate_7_to_8() -> String {
    format!(
        "ALTER TABLE series ADD COLUMN library TEXT GENERATED ALWAYS AS ({LIBRARY_OF_PATH}) VIRTUAL;
         CREATE INDEX series_library ON series(library);"
    )
}

/// The collection a series is in, from its path (see `crate::libraries`):
/// the folders between the library and the series, else `''`. The parent
/// folder is the path with its last part cut (`rtrim` by every character
/// but `/`).
const COLLECTION_OF_PATH: &str = "CASE WHEN substr(path, 1, 1) = char(0) OR instr(path, '/') = 0 \
     THEN '' ELSE substr(rtrim(path, replace(path, '/', '')), instr(path, '/') + 1, \
     max(length(rtrim(path, replace(path, '/', ''))) - instr(path, '/') - 1, 0)) END";

/// Schema 9 to 10: series get their collection, and an index that pages a
/// library's shelf without reading all of it.
fn migrate_9_to_10() -> String {
    format!(
        "ALTER TABLE series ADD COLUMN collection TEXT GENERATED ALWAYS AS ({COLLECTION_OF_PATH}) VIRTUAL;
         CREATE INDEX series_shelf ON series(library, collection, name COLLATE natsort, path COLLATE natsort);
         CREATE INDEX series_names ON series(library, name COLLATE natsort, path COLLATE natsort);"
    )
}

/// Schema 8 to 9: the search index, filled by the next scan.
const MIGRATE_8_TO_9: &str = "CREATE VIRTUAL TABLE search USING fts5(
    name, people, genres, tags, kind UNINDEXED, ref UNINDEXED, tokenize = 'trigram');";

/// Schema 10 to 11: EPUB chapters and contents. Every folder is read again
/// at the next scan, so EPUB files, not books before, are found; books
/// already known are not read again.
const MIGRATE_10_TO_11: &str = "CREATE TABLE chapters (
    book_id INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE, number INTEGER NOT NULL,
    hash BLOB NOT NULL, length INTEGER NOT NULL, title TEXT NOT NULL,
    PRIMARY KEY (book_id, number)) WITHOUT ROWID;
CREATE TABLE toc (
    book_id INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE, pos INTEGER NOT NULL,
    level INTEGER NOT NULL, title TEXT NOT NULL, chapter INTEGER NOT NULL, anchor TEXT,
    PRIMARY KEY (book_id, pos)) WITHOUT ROWID;
UPDATE folders SET mtime = NULL;";

/// Tables of earlier schema versions, dropped when upgrading: until accounts
/// exist the catalogue holds nothing that the next scan cannot rebuild.
const DROP_OLD: &str = "
DROP TABLE IF EXISTS toc;
DROP TABLE IF EXISTS chapters;
DROP TABLE IF EXISTS pages;
DROP TABLE IF EXISTS book_terms;
DROP TABLE IF EXISTS books;
DROP TABLE IF EXISTS series;
DROP TABLE IF EXISTS scan_errors;
DROP TABLE IF EXISTS folders;
DROP TABLE IF EXISTS jobs;
DROP TABLE IF EXISTS variants;
DROP TABLE IF EXISTS variant_pages;
DROP TABLE IF EXISTS search;
DROP TABLE IF EXISTS sqlite_sequence;
";

/// A file's size and modification time (nanoseconds since the epoch): if
/// both are unchanged, the file is not read again.
pub type Stamp = (u64, i64);

pub struct Catalog {
    conn: Connection,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Series {
    pub id: i64,
    pub path: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Book {
    pub id: i64,
    pub series_id: i64,
    pub path: String,
    pub name: String,
    pub page_count: i64,
    /// Unix seconds when Koma-1 first saw it; kept through moves and renames.
    pub added: i64,
    /// Bytes: the book file's, or a photo folder's images together.
    pub size: u64,
    /// A photo folder (its path is a folder of images), not one file.
    pub photos: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub number: i64,
    pub hash: blake3::Hash,
    pub format: String,
    pub length: u64,
    /// Offset in the book file (or in `file`), or `None` when the page is in
    /// the blob store.
    pub offset: Option<u64>,
    /// The file holding the page, relative to the library, when it is not
    /// the book file: each page of a photo folder is a file of its own.
    pub file: Option<String>,
    /// Width and height as displayed, read from the header at ingest;
    /// `None` if the header could not be read.
    pub size: Option<Size>,
}

/// A series as a row of the library's table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesRow {
    pub series: Series,
    pub books: i64,
    pub pages: i64,
    /// Unix seconds: when its first book was first seen.
    pub added: i64,
    /// Its first two authors, then its first three genres, comma-separated.
    pub authors: String,
    pub genres: String,
}

/// The page shown as a book's or series' cover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cover {
    pub hash: blake3::Hash,
    /// The page's own format (file extension).
    pub format: String,
    /// Formats (`jpg`, `webp`) of its variant at the asked width, if made.
    pub variant_formats: Vec<String>,
}

/// Where a page's bytes are, found by hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageLocation {
    /// The file to read at `offset`: the book file, or a photo folder's image.
    pub book_path: String,
    pub format: String,
    pub length: u64,
    pub offset: Option<u64>,
}

/// Where a book is: its path and the series it belongs to.
pub struct Place<'a> {
    pub path: &'a str,
    pub folder: &'a str,
    pub name: &'a str,
    pub series_path: &'a str,
    pub series_name: &'a str,
}

/// A book as the scanner found it, ready to store.
pub struct NewBook<'a> {
    pub series_path: &'a str,
    pub series_name: &'a str,
    pub path: &'a str,
    pub folder: &'a str,
    pub name: &'a str,
    pub size: u64,
    pub mtime: i64,
    pub pages: &'a [Page],
    /// An EPUB's chapters and contents; empty for a book of pages.
    pub chapters: &'a [Chapter],
    pub toc: &'a [TocRow],
    pub details: &'a Details,
}

/// A chapter of an EPUB, cleaned (see [`crate::epub::clean`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chapter {
    /// 0-based reading order.
    pub number: i64,
    /// The cleaned chapter is the blob `<hash>.html`.
    pub hash: blake3::Hash,
    pub length: u64,
    pub title: String,
}

/// An entry of an EPUB's contents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TocRow {
    pub level: u32,
    pub title: String,
    /// 0-based chapter number.
    pub chapter: i64,
    pub anchor: Option<String>,
}

/// What a book says about itself: its metadata files, with gaps filled from
/// its file name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Details {
    pub title: String,
    pub number: Option<f64>,
    /// `"chapter"` or `"volume"`.
    pub kind: Option<&'static str>,
    pub summary: String,
    pub year: Option<i32>,
    pub language: String,
    /// `"ltr"` or `"rtl"`.
    pub direction: Option<&'static str>,
    /// 0-based page to show as the cover.
    pub cover_page: Option<u32>,
    pub authors: Vec<String>,
    pub publishers: Vec<String>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
}

/// [`Details`] as stored, read back by [`Catalog::details`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StoredDetails {
    pub title: String,
    pub number: Option<f64>,
    pub kind: Option<String>,
    pub summary: String,
    pub year: Option<i32>,
    pub language: String,
    pub direction: Option<String>,
    pub cover_page: Option<u32>,
    pub authors: Vec<String>,
    pub publishers: Vec<String>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
}

const TERM_KINDS: [&str; 4] = ["author", "publisher", "genre", "tag"];

impl Catalog {
    /// Opens (creating if needed) the catalogue for writing.
    pub fn open(path: &Path) -> Result<Self, Error> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let catalog = Self::configure(conn)?;
        catalog.migrate()?;
        Ok(catalog)
    }

    /// Opens an existing catalogue read-only, as `koma serve` does.
    pub fn open_read_only(path: &Path) -> Result<Self, Error> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let catalog = Self::configure(conn)?;
        let version: i64 = catalog
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version != SCHEMA_VERSION {
            return Err(Error::InvalidQuery);
        }
        Ok(catalog)
    }

    fn configure(conn: Connection) -> Result<Self, Error> {
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.create_collation("natsort", crate::natsort::compare_paths)?;
        Ok(Self { conn })
    }

    fn migrate(&self) -> Result<(), Error> {
        let version: i64 = self
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            // From 7 on the catalogue is upgraded in place, one step at a
            // time: remaking page variants is too costly to start over.
            7..=10 => {
                let mut steps = String::new();
                if version == 7 {
                    steps.push_str(&migrate_7_to_8());
                }
                if version <= 8 {
                    steps.push_str(MIGRATE_8_TO_9);
                }
                if version <= 9 {
                    steps.push_str(&migrate_9_to_10());
                }
                steps.push_str(MIGRATE_10_TO_11);
                self.conn.execute_batch(&format!(
                    "BEGIN; {steps} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
                Ok(())
            }
            0..SCHEMA_VERSION => {
                // Before 7, nothing was kept that a scan cannot rebuild.
                let drop = if version > 0 { DROP_OLD } else { "" };
                let schema = SCHEMA
                    .replace("{LIBRARY_OF_PATH}", LIBRARY_OF_PATH)
                    .replace("{COLLECTION_OF_PATH}", COLLECTION_OF_PATH);
                self.conn.execute_batch(&format!(
                    "BEGIN; {drop} {schema} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
                Ok(())
            }
            SCHEMA_VERSION => Ok(()),
            _ => Err(Error::InvalidQuery),
        }
    }

    /// Size and modification time recorded for a present book, if it is known.
    pub fn book_stamp(&self, path: &str) -> Result<Option<Stamp>, Error> {
        self.conn
            .query_row(
                "SELECT size, mtime FROM books WHERE path = ?1 AND gone IS NULL",
                [path],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
            )
            .optional()
    }

    /// Stores a book and its pages, replacing what was known about that
    /// file. A book already known at this path (present or gone) keeps its
    /// id and date added.
    pub fn replace_book(&mut self, book: &NewBook<'_>, now: i64) -> Result<i64, Error> {
        let tx = self.conn.transaction()?;
        let before: Option<(i64, i64)> = tx
            .query_row(
                "SELECT id, series_id FROM books WHERE path = ?1",
                [book.path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let series_id = place_in_series(&tx, book.series_path, book.series_name, before)?;
        let book_id: i64 = tx.query_row(
            "INSERT INTO books (series_id, path, folder, name, size, mtime, page_count, added)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (path) DO UPDATE SET series_id = excluded.series_id,
                 folder = excluded.folder, name = excluded.name, size = excluded.size,
                 mtime = excluded.mtime, page_count = excluded.page_count, gone = NULL
             RETURNING id",
            params![
                series_id,
                book.path,
                book.folder,
                book.name,
                book.size as i64,
                book.mtime,
                if book.chapters.is_empty() {
                    book.pages.len()
                } else {
                    book.chapters.len()
                } as i64,
                now
            ],
            |r| r.get(0),
        )?;
        let d = book.details;
        tx.execute(
            "UPDATE books SET title = ?2, number = ?3, kind = ?4, summary = ?5, year = ?6,
                 language = ?7, direction = ?8, cover_page = ?9
             WHERE id = ?1",
            params![
                book_id,
                d.title,
                d.number,
                d.kind,
                d.summary,
                d.year,
                d.language,
                d.direction,
                d.cover_page
            ],
        )?;
        tx.execute("DELETE FROM book_terms WHERE book_id = ?1", [book_id])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO book_terms (book_id, kind, name, pos) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (kind, names) in
                TERM_KINDS
                    .iter()
                    .zip([&d.authors, &d.publishers, &d.genres, &d.tags])
            {
                for (pos, name) in names.iter().enumerate() {
                    insert.execute(params![book_id, kind, name, pos as i64])?;
                }
            }
        }
        tx.execute("DELETE FROM pages WHERE book_id = ?1", [book_id])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO pages (book_id, number, hash, format, length, offset, file, width, height)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for page in book.pages {
                insert.execute(params![
                    book_id,
                    page.number,
                    page.hash.as_bytes(),
                    page.format,
                    page.length as i64,
                    page.offset.map(|o| o as i64),
                    page.file,
                    page.size.map(|s| s.width),
                    page.size.map(|s| s.height),
                ])?;
            }
        }
        tx.execute("DELETE FROM chapters WHERE book_id = ?1", [book_id])?;
        tx.execute("DELETE FROM toc WHERE book_id = ?1", [book_id])?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO chapters (book_id, number, hash, length, title) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for c in book.chapters {
                insert.execute(params![
                    book_id,
                    c.number,
                    c.hash.as_bytes(),
                    c.length as i64,
                    c.title
                ])?;
            }
            let mut insert = tx.prepare(
                "INSERT INTO toc (book_id, pos, level, title, chapter, anchor) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for (pos, t) in book.toc.iter().enumerate() {
                insert.execute(params![
                    book_id, pos as i64, t.level, t.title, t.chapter, t.anchor
                ])?;
            }
        }
        tx.execute("DELETE FROM scan_errors WHERE path = ?1", [book.path])?;
        // The series may have been left empty by a book that changed series.
        prune_series(&tx)?;
        tx.commit()?;
        Ok(book_id)
    }

    /// The details stored for the book at this path.
    pub fn details(&self, path: &str) -> Result<Option<StoredDetails>, Error> {
        let found = self
            .conn
            .query_row(
                "SELECT id, title, number, kind, summary, year, language, direction, cover_page
                 FROM books WHERE path = ?1",
                [path],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        StoredDetails {
                            title: r.get(1)?,
                            number: r.get(2)?,
                            kind: r.get(3)?,
                            summary: r.get(4)?,
                            year: r.get(5)?,
                            language: r.get(6)?,
                            direction: r.get(7)?,
                            cover_page: r.get(8)?,
                            ..StoredDetails::default()
                        },
                    ))
                },
            )
            .optional()?;
        let Some((id, mut details)) = found else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare_cached(
            "SELECT kind, name FROM book_terms WHERE book_id = ?1 ORDER BY kind, pos",
        )?;
        let rows = stmt.query_map([id], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?;
        for row in rows {
            let (kind, name) = row?;
            match kind.as_str() {
                "author" => details.authors.push(name),
                "publisher" => details.publishers.push(name),
                "genre" => details.genres.push(name),
                _ => details.tags.push(name),
            }
        }
        Ok(Some(details))
    }

    /// The book known at this path: its id, stamp, and whether it is gone.
    pub fn book_at(&self, path: &str) -> Result<Option<(i64, Stamp, bool)>, Error> {
        self.conn
            .query_row(
                "SELECT id, size, mtime, gone IS NOT NULL FROM books WHERE path = ?1",
                [path],
                |r| {
                    Ok((
                        r.get(0)?,
                        (r.get::<_, i64>(1)? as u64, r.get(2)?),
                        r.get(3)?,
                    ))
                },
            )
            .optional()
    }

    /// A gone book that a book found at `path` with this stamp is most
    /// likely to be, moved or renamed outside Koma-1: a move or rename keeps
    /// size and modification time. One with the same file name wins;
    /// otherwise there must be exactly one candidate.
    pub fn gone_match(&self, path: &str, stamp: Stamp) -> Result<Option<i64>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path FROM books WHERE gone IS NOT NULL AND size = ?1 AND mtime = ?2
             ORDER BY gone DESC, id",
        )?;
        let found: Vec<(i64, String)> = stmt
            .query_map(params![stamp.0 as i64, stamp.1], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<Result<_, _>>()?;
        let name = last_part(path);
        Ok(found
            .iter()
            .find(|(_, p)| last_part(p) == name)
            .or(if found.len() == 1 {
                found.first()
            } else {
                None
            })
            .map(|(id, _)| *id))
    }

    /// Moves a known book (present or gone) to a new place without reading
    /// it again: its id, pages, progress and date added stay. A book that
    /// leaves a series where no other book is present takes the series (and
    /// its id) along, so a renamed or moved series folder keeps its id too.
    pub fn relink(&mut self, id: i64, to: &Place<'_>) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        let old_series: i64 =
            tx.query_row("SELECT series_id FROM books WHERE id = ?1", [id], |r| {
                r.get(0)
            })?;
        let series_id =
            place_in_series(&tx, to.series_path, to.series_name, Some((id, old_series)))?;
        // Whatever was known at the new path is replaced by the moved book.
        tx.execute(
            "DELETE FROM books WHERE path = ?1 AND id != ?2",
            params![to.path, id],
        )?;
        tx.execute(
            "UPDATE books SET series_id = ?1, path = ?2, folder = ?3, name = ?4, gone = NULL
             WHERE id = ?5",
            params![series_id, to.path, to.folder, to.name, id],
        )?;
        tx.execute("DELETE FROM scan_errors WHERE path = ?1", [to.path])?;
        prune_series(&tx)?;
        tx.commit()
    }

    /// Marks a book whose file vanished (or can no longer be read) as gone:
    /// it disappears from the library but is remembered for
    /// [`GONE_GRACE_SECS`]. Returns whether a present book was known there.
    pub fn vanish_book(&mut self, path: &str, now: i64) -> Result<bool, Error> {
        let n = self.conn.execute(
            "UPDATE books SET gone = ?2 WHERE path = ?1 AND gone IS NULL",
            params![path, now],
        )?;
        Ok(n > 0)
    }

    /// Forgets books gone since before `before` for good, and series left
    /// with no books. Returns how many books were forgotten.
    pub fn purge_gone(&mut self, before: i64) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let n = tx.execute("DELETE FROM books WHERE gone < ?1", [before])?;
        prune_series(&tx)?;
        tx.commit()?;
        Ok(n)
    }

    /// Forgets a folder that is gone: its books are marked gone (see
    /// [`Catalog::vanish_book`]), what else is known under it is dropped.
    /// Returns how many present books vanished.
    pub fn remove_tree(&mut self, folder: &str, now: i64) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        // Inside `folder`: the folder itself, or anything under `folder/`
        // (paths that sort from "folder/" up to, not including, "folder0").
        let under = "(?1 = col OR (col >= ?1 || '/' AND col < ?1 || '0'))";
        let removed = tx.execute(
            &format!(
                "UPDATE books SET gone = ?2 WHERE gone IS NULL AND {}",
                under.replace("col", "folder")
            ),
            params![folder, now],
        )?;
        tx.execute(
            &format!(
                "DELETE FROM scan_errors WHERE {}",
                under.replace("col", "folder")
            ),
            [folder],
        )?;
        tx.execute(
            &format!(
                "DELETE FROM scan_errors WHERE path = ?1 OR {}",
                under.replace("col", "path")
            ),
            [folder],
        )?;
        tx.execute(
            &format!("DELETE FROM folders WHERE {}", under.replace("col", "path")),
            [folder],
        )?;
        prune_series(&tx)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Records a book or folder that could not be read, with the size and
    /// modification time it had, so it is retried only once it changes.
    pub fn record_error(
        &self,
        path: &str,
        folder: &str,
        stamp: Option<Stamp>,
        error: &str,
    ) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO scan_errors (path, folder, size, mtime, error) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (path) DO UPDATE SET folder = excluded.folder, size = excluded.size,
                 mtime = excluded.mtime, error = excluded.error",
            params![
                path,
                folder,
                stamp.map(|s| s.0 as i64),
                stamp.map(|s| s.1),
                error
            ],
        )?;
        Ok(())
    }

    pub fn remove_error(&self, path: &str) -> Result<(), Error> {
        self.conn
            .execute("DELETE FROM scan_errors WHERE path = ?1", [path])?;
        Ok(())
    }

    /// Size and modification time of a file that failed, if it is recorded.
    pub fn error_stamp(&self, path: &str) -> Result<Option<Stamp>, Error> {
        self.conn
            .query_row(
                "SELECT size, mtime FROM scan_errors WHERE path = ?1 AND size IS NOT NULL",
                [path],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
            )
            .optional()
    }

    /// Books directly in `folder`, with their size and modification time.
    pub fn books_in(&self, folder: &str) -> Result<Vec<(String, u64, i64)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT path, size, mtime FROM books WHERE folder = ?1 AND gone IS NULL",
        )?;
        let rows = stmt.query_map([folder], |r| {
            Ok((r.get(0)?, r.get::<_, i64>(1)? as u64, r.get(2)?))
        })?;
        rows.collect()
    }

    /// Failed files and folders directly in `folder`, with the size and
    /// modification time of files.
    pub fn errors_in(&self, folder: &str) -> Result<Vec<(String, Option<Stamp>)>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path, size, mtime FROM scan_errors WHERE folder = ?1")?;
        let rows = stmt.query_map([folder], |r| {
            let size: Option<i64> = r.get(1)?;
            let mtime: Option<i64> = r.get(2)?;
            Ok((r.get(0)?, size.zip(mtime).map(|(s, m)| (s as u64, m))))
        })?;
        rows.collect()
    }

    /// A folder's modification time when it was last read in full: `None`
    /// if it was never read, `Some(None)` if it must be read again.
    pub fn folder_mtime(&self, path: &str) -> Result<Option<Option<i64>>, Error> {
        self.conn
            .prepare_cached("SELECT mtime FROM folders WHERE path = ?1")?
            .query_row([path], |r| r.get(0))
            .optional()
    }

    /// The known folders directly inside `path`.
    pub fn subfolders(&self, path: &str) -> Result<Vec<String>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path FROM folders WHERE parent = ?1")?;
        let rows = stmt.query_map([path], |r| r.get(0))?;
        rows.collect()
    }

    /// Records that a folder was read in full when it had this modification
    /// time (`None`: read it again next time).
    pub fn set_folder(
        &self,
        path: &str,
        parent: Option<&str>,
        mtime: Option<i64>,
    ) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO folders (path, parent, mtime) VALUES (?1, ?2, ?3)
             ON CONFLICT (path) DO UPDATE SET parent = excluded.parent, mtime = excluded.mtime",
            params![path, parent, mtime],
        )?;
        Ok(())
    }

    /// Queues a job, unless the same one is already waiting or running.
    /// Returns whether it was added.
    pub fn enqueue(&self, kind: JobKind, path: &str) -> Result<bool, Error> {
        let added = self.conn.execute(
            "INSERT INTO jobs (kind, path) VALUES (?1, ?2) ON CONFLICT (kind, path) DO NOTHING",
            params![kind.as_str(), path],
        )?;
        Ok(added > 0)
    }

    /// Takes the oldest waiting job and marks it running, counting the attempt.
    /// Variants jobs wait for every other kind: new books are readable first.
    pub fn claim_job(&self) -> Result<Option<Job>, Error> {
        self.conn
            .prepare_cached(
                "UPDATE jobs SET running = 1, attempts = attempts + 1
                 WHERE id = (SELECT id FROM jobs WHERE running = 0
                             ORDER BY kind = 'variants', id LIMIT 1)
                 RETURNING id, kind, path, attempts",
            )?
            .query_row([], |r| {
                let kind: String = r.get(1)?;
                Ok(Job {
                    id: r.get(0)?,
                    kind: JobKind::parse(&kind).ok_or_else(|| {
                        Error::FromSqlConversionFailure(
                            1,
                            rusqlite::types::Type::Text,
                            format!("unknown job kind: {kind}").into(),
                        )
                    })?,
                    path: r.get(2)?,
                    attempts: r.get(3)?,
                })
            })
            .optional()
    }

    /// Removes a job that is done (or given up).
    pub fn finish_job(&self, id: i64) -> Result<(), Error> {
        self.conn.execute("DELETE FROM jobs WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Puts a job claimed but not worked on back in the queue, without
    /// counting the attempt: the worker is stopping.
    pub fn release_job(&self, id: i64) -> Result<(), Error> {
        self.conn.execute(
            "UPDATE jobs SET running = 0, attempts = attempts - 1 WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// Puts jobs that were running when the worker last stopped back in the
    /// queue. Returns how many there were.
    pub fn requeue_running(&self) -> Result<usize, Error> {
        self.conn
            .execute("UPDATE jobs SET running = 0 WHERE running = 1", [])
    }

    /// Jobs waiting or running.
    pub fn job_count(&self) -> Result<i64, Error> {
        self.conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |r| r.get(0))
    }

    /// Files the last scan could not read, with the reason.
    pub fn errors(&self) -> Result<Vec<(String, String)>, Error> {
        let mut stmt = self
            .conn
            .prepare("SELECT path, error FROM scan_errors ORDER BY path COLLATE natsort")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// All series with a book present, in natural order of their names.
    pub fn series(&self) -> Result<Vec<Series>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path, name FROM series
             WHERE EXISTS (SELECT 1 FROM books WHERE series_id = series.id AND gone IS NULL)
             ORDER BY name COLLATE natsort, path COLLATE natsort",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(Series {
                id: r.get(0)?,
                path: r.get(1)?,
                name: r.get(2)?,
            })
        })?;
        rows.collect()
    }

    /// All series with how many books each has, in natural order of their names.
    pub fn series_with_counts(&self) -> Result<Vec<(Series, i64)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT series.id, series.path, series.name, COUNT(books.id)
             FROM series JOIN books ON books.series_id = series.id AND books.gone IS NULL
             GROUP BY series.id
             ORDER BY series.name COLLATE natsort, series.path COLLATE natsort",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                Series {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    name: r.get(2)?,
                },
                r.get(3)?,
            ))
        })?;
        rows.collect()
    }

    /// Every library with a present book, as `(folder, series count)`:
    /// `''` (the default library) first, then folders in natural order.
    pub fn library_keys(&self) -> Result<Vec<(String, i64)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT library, COUNT(*) FROM series
             WHERE EXISTS (SELECT 1 FROM books WHERE series_id = series.id AND gone IS NULL)
             GROUP BY library ORDER BY library <> '', library COLLATE natsort",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.collect()
    }

    /// The series of one library (`''`: the default one) with how many
    /// books each has, in natural order of their names.
    pub fn series_in_library(&self, library: &str) -> Result<Vec<(Series, i64)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT series.id, series.path, series.name, COUNT(books.id)
             FROM series JOIN books ON books.series_id = series.id AND books.gone IS NULL
             WHERE series.library = ?1
             GROUP BY series.id
             ORDER BY series.name COLLATE natsort, series.path COLLATE natsort",
        )?;
        let rows = stmt.query_map([library], |r| {
            Ok((
                Series {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    name: r.get(2)?,
                },
                r.get(3)?,
            ))
        })?;
        rows.collect()
    }

    /// A book's cover: its `cover_page` (from its metadata) if it has that
    /// many pages, else its first page; with the formats of its variant
    /// `width` pixels wide made under `version`, if any.
    pub fn book_cover(
        &self,
        book_id: i64,
        width: u32,
        version: i64,
    ) -> Result<Option<Cover>, Error> {
        self.conn
            .prepare_cached(
                "SELECT pages.hash, pages.format,
                        (SELECT group_concat(v.format) FROM variants v
                         JOIN variant_pages vp ON vp.hash = v.hash AND vp.version = ?3
                         WHERE v.hash = pages.hash AND v.width = ?2)
                 FROM books JOIN pages ON pages.book_id = books.id
                  AND pages.number = CASE WHEN books.cover_page < books.page_count
                                          THEN books.cover_page ELSE 0 END
                 WHERE books.id = ?1",
            )?
            .query_row(params![book_id, width, version], |r| {
                let formats: Option<String> = r.get(2)?;
                Ok(Cover {
                    hash: hash_from_row(r, 0)?,
                    format: r.get(1)?,
                    variant_formats: formats
                        .map(|f| f.split(',').map(str::to_owned).collect())
                        .unwrap_or_default(),
                })
            })
            .optional()
    }

    /// The first present book of a series, in natural order of paths.
    pub fn first_book(&self, series_id: i64) -> Result<Option<i64>, Error> {
        self.conn
            .prepare_cached(
                "SELECT id FROM books WHERE series_id = ?1 AND gone IS NULL
                 ORDER BY path COLLATE natsort LIMIT 1",
            )?
            .query_row([series_id], |r| r.get(0))
            .optional()
    }

    /// Refills the search index from the catalogue.
    pub fn rebuild_search(&mut self) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "DELETE FROM search;
             INSERT INTO search (name, people, genres, tags, kind, ref)
             SELECT series.name,
                    coalesce(group_concat(DISTINCT CASE WHEN t.kind IN ('author', 'publisher')
                                                   THEN t.name END), ''),
                    coalesce(group_concat(DISTINCT CASE WHEN t.kind = 'genre' THEN t.name END), ''),
                    coalesce(group_concat(DISTINCT CASE WHEN t.kind = 'tag' THEN t.name END), ''),
                    's', series.id
             FROM series JOIN books ON books.series_id = series.id AND books.gone IS NULL
             LEFT JOIN book_terms t ON t.book_id = books.id
             GROUP BY series.id;
             INSERT INTO search (name, people, genres, tags, kind, ref)
             SELECT CASE WHEN title <> '' THEN title ELSE name END, '', '', '', 'b', id
             FROM books WHERE gone IS NULL;",
        )?;
        tx.commit()
    }

    /// Whether the search index is empty while there are books: a catalogue
    /// just upgraded to the index, which the next scan fills.
    pub fn search_needs_filling(&self) -> Result<bool, Error> {
        self.conn.query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM search)
                AND EXISTS (SELECT 1 FROM books WHERE gone IS NULL)",
            [],
            |r| r.get(0),
        )
    }

    /// Runs a search built by [`crate::search`]: `fts` is an FTS5 query (or
    /// none), each of `likes` a `(column, LIKE pattern)` every hit must also
    /// match (for terms too short for trigrams), all in `library` if given.
    /// Returns `(kind, id)`, best first: `'s'` for series, `'b'` for books.
    pub fn search(
        &self,
        fts: Option<&str>,
        likes: &[(&str, String)],
        library: Option<&str>,
        limit: usize,
    ) -> Result<Vec<(String, i64)>, Error> {
        let mut sql = String::from(
            "SELECT search.kind, search.ref FROM search
             LEFT JOIN series ON search.kind = 's' AND series.id = search.ref
             LEFT JOIN books ON search.kind = 'b' AND books.id = search.ref
             LEFT JOIN series bs ON bs.id = books.series_id
             WHERE 1",
        );
        let mut args: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(q) = fts {
            args.push(q.to_owned().into());
            sql.push_str(&format!(" AND search MATCH ?{}", args.len()));
        }
        for (column, pattern) in likes {
            args.push(pattern.clone().into());
            let n = args.len();
            // Only the four text columns, never the kind or id.
            let cols: &[&str] = match *column {
                "people" | "genres" | "tags" | "name" => std::slice::from_ref(column),
                _ => &["name", "people", "genres", "tags"],
            };
            let any: Vec<String> = cols
                .iter()
                .map(|c| format!("search.{c} LIKE ?{n} ESCAPE '\\'"))
                .collect();
            sql.push_str(&format!(" AND ({})", any.join(" OR ")));
        }
        if let Some(l) = library {
            args.push(l.to_owned().into());
            sql.push_str(&format!(
                " AND coalesce(series.library, bs.library) = ?{}",
                args.len()
            ));
        }
        sql.push_str(if fts.is_some() {
            " ORDER BY search.kind DESC, rank"
        } else {
            " ORDER BY search.kind DESC, search.name COLLATE natsort"
        });
        args.push((limit as i64).into());
        sql.push_str(&format!(" LIMIT ?{}", args.len()));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect()
    }

    /// People, genres and tags whose name contains `pattern` (a LIKE
    /// pattern), as `(kind, name, books)`, most books first.
    pub fn matching_terms(
        &self,
        pattern: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, i64)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT kind, name, COUNT(DISTINCT book_id) AS n FROM book_terms
             WHERE name LIKE ?1 ESCAPE '\\'
               AND EXISTS (SELECT 1 FROM books WHERE id = book_id AND gone IS NULL)
             GROUP BY kind, name ORDER BY n DESC, name COLLATE natsort LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![pattern, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect()
    }

    /// Every series of a library (or of one of its collections) with what its
    /// table row shows but people and genres ([`Catalog::series_terms`]),
    /// in natural order of names.
    pub fn series_rows(
        &self,
        library: &str,
        collection: Option<&str>,
    ) -> Result<Vec<SeriesRow>, Error> {
        let mut stmt = self.conn.prepare_cached(
            // Book counts in one pass over books, then the series in index
            // order (series_names): no sort of every name through the
            // natsort collation on each request.
            "WITH b AS MATERIALIZED (
                 SELECT series_id, COUNT(*) AS n, SUM(page_count) AS pages, MIN(added) AS added
                 FROM books WHERE gone IS NULL GROUP BY series_id)
             SELECT s.id, s.path, s.name, b.n, b.pages, b.added
             FROM series s JOIN b ON b.series_id = s.id
             WHERE s.library = ?1 AND (?2 IS NULL OR s.collection = ?2)
             ORDER BY s.name COLLATE natsort, s.path COLLATE natsort",
        )?;
        let rows = stmt.query_map(params![library, collection], |r| {
            Ok(SeriesRow {
                series: Series {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    name: r.get(2)?,
                },
                books: r.get(3)?,
                pages: r.get(4)?,
                added: r.get(5)?,
                authors: String::new(),
                genres: String::new(),
            })
        })?;
        rows.collect()
    }

    /// Fills in each row's first two authors and first three genres.
    pub fn series_terms(&self, rows: &mut [SeriesRow]) -> Result<(), Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT t.name FROM book_terms t JOIN books b ON b.id = t.book_id
             WHERE b.series_id = ?1 AND b.gone IS NULL AND t.kind = ?2
             GROUP BY t.name ORDER BY MIN(t.pos), t.name LIMIT ?3",
        )?;
        for row in rows {
            let mut list = |kind: &str, limit: i64| -> Result<String, Error> {
                let names = stmt
                    .query_map(params![row.series.id, kind, limit], |r| {
                        r.get::<_, String>(0)
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(names.join(", "))
            };
            row.authors = list("author", 2)?;
            row.genres = list("genre", 3)?;
        }
        Ok(())
    }

    /// The collections of a library, as `(name, series, books, first
    /// series' id)`, in natural order of names.
    pub fn collections(&self, library: &str) -> Result<Vec<(String, i64, i64, i64)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT collection, COUNT(*), SUM(n), MIN(CASE WHEN rn = 1 THEN id END) FROM (
                 SELECT series.id, series.collection,
                        (SELECT COUNT(*) FROM books WHERE series_id = series.id AND gone IS NULL) AS n,
                        ROW_NUMBER() OVER (PARTITION BY series.collection
                            ORDER BY series.name COLLATE natsort, series.path COLLATE natsort) AS rn
                 FROM series
                 WHERE series.library = ?1 AND series.collection <> ''
                   AND EXISTS (SELECT 1 FROM books WHERE series_id = series.id AND gone IS NULL))
             GROUP BY collection ORDER BY collection COLLATE natsort",
        )?;
        let rows = stmt.query_map([library], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
        rows.collect()
    }

    /// One page of the series of a library shelf: those in `collection`
    /// (`''`: those in no collection), natural order, with their book
    /// counts; and how many there are in all.
    pub fn shelf_series(
        &self,
        library: &str,
        collection: &str,
        offset: usize,
        limit: usize,
    ) -> Result<(Vec<(Series, i64)>, usize), Error> {
        let total: i64 = self
            .conn
            .prepare_cached(
                "SELECT COUNT(*) FROM series WHERE library = ?1 AND collection = ?2
               AND EXISTS (SELECT 1 FROM books WHERE series_id = series.id AND gone IS NULL)",
            )?
            .query_row(params![library, collection], |r| r.get(0))?;
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path, name,
                    (SELECT COUNT(*) FROM books WHERE series_id = series.id AND gone IS NULL) AS n
             FROM series WHERE library = ?1 AND collection = ?2 AND n > 0
             ORDER BY name COLLATE natsort, path COLLATE natsort LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(
            params![library, collection, limit as i64, offset as i64],
            |r| {
                Ok((
                    Series {
                        id: r.get(0)?,
                        path: r.get(1)?,
                        name: r.get(2)?,
                    },
                    r.get(3)?,
                ))
            },
        )?;
        Ok((rows.collect::<Result<_, _>>()?, total as usize))
    }

    pub fn series_by_id(&self, id: i64) -> Result<Option<Series>, Error> {
        self.conn
            .prepare_cached("SELECT id, path, name FROM series WHERE id = ?1")?
            .query_row([id], |r| {
                Ok(Series {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    name: r.get(2)?,
                })
            })
            .optional()
    }

    pub fn book(&self, id: i64) -> Result<Option<Book>, Error> {
        self.conn
            .prepare_cached(
                "SELECT id, series_id, path, name, page_count, added, size, folder = path FROM books
                 WHERE id = ?1 AND gone IS NULL",
            )?
            .query_row([id], |r| {
                Ok(Book {
                    id: r.get(0)?,
                    series_id: r.get(1)?,
                    path: r.get(2)?,
                    name: r.get(3)?,
                    page_count: r.get(4)?,
                    added: r.get(5)?,
                    size: r.get::<_, i64>(6)? as u64,
                    photos: r.get(7)?,
                })
            })
            .optional()
    }

    /// The books of a series, in natural order of their file names.
    pub fn books(&self, series_id: i64) -> Result<Vec<Book>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, series_id, path, name, page_count, added, size, folder = path FROM books
             WHERE series_id = ?1 AND gone IS NULL ORDER BY path COLLATE natsort",
        )?;
        let rows = stmt.query_map([series_id], |r| {
            Ok(Book {
                id: r.get(0)?,
                series_id: r.get(1)?,
                path: r.get(2)?,
                name: r.get(3)?,
                page_count: r.get(4)?,
                added: r.get(5)?,
                size: r.get::<_, i64>(6)? as u64,
                photos: r.get(7)?,
            })
        })?;
        rows.collect()
    }

    /// The pages of a book, in reading order.
    pub fn pages(&self, book_id: i64) -> Result<Vec<Page>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT number, hash, format, length, offset, file, width, height FROM pages
             WHERE book_id = ?1 ORDER BY number",
        )?;
        let rows = stmt.query_map([book_id], |r| {
            Ok(Page {
                number: r.get(0)?,
                hash: hash_from_row(r, 1)?,
                format: r.get(2)?,
                length: r.get::<_, i64>(3)? as u64,
                offset: r.get::<_, Option<i64>>(4)?.map(|o| o as u64),
                file: r.get(5)?,
                size: match (r.get::<_, Option<u32>>(6)?, r.get::<_, Option<u32>>(7)?) {
                    (Some(width), Some(height)) => Some(Size { width, height }),
                    _ => None,
                },
            })
        })?;
        rows.collect()
    }

    /// One chapter of an EPUB, 0-based.
    pub fn chapter(&self, book_id: i64, number: i64) -> Result<Option<Chapter>, Error> {
        self.conn
            .prepare_cached(
                "SELECT number, hash, length, title FROM chapters WHERE book_id = ?1 AND number = ?2",
            )?
            .query_row(params![book_id, number], |r| {
                Ok(Chapter {
                    number: r.get(0)?,
                    hash: hash_from_row(r, 1)?,
                    length: r.get::<_, i64>(2)? as u64,
                    title: r.get(3)?,
                })
            })
            .optional()
    }

    /// An EPUB's contents, in order.
    pub fn toc(&self, book_id: i64) -> Result<Vec<TocRow>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT level, title, chapter, anchor FROM toc WHERE book_id = ?1 ORDER BY pos",
        )?;
        let rows = stmt.query_map([book_id], |r| {
            Ok(TocRow {
                level: r.get(0)?,
                title: r.get(1)?,
                chapter: r.get(2)?,
                anchor: r.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Where to read a page with this hash, preferring one inside an archive.
    pub fn page_by_hash(&self, hash: &blake3::Hash) -> Result<Option<PageLocation>, Error> {
        self.conn
            .prepare_cached(
                "SELECT COALESCE(pages.file, books.path), pages.format, pages.length, pages.offset
                 FROM pages JOIN books ON books.id = pages.book_id AND books.gone IS NULL
                 WHERE pages.hash = ?1 ORDER BY pages.offset IS NULL LIMIT 1",
            )?
            .query_row([hash.as_bytes()], |r| {
                Ok(PageLocation {
                    book_path: r.get(0)?,
                    format: r.get(1)?,
                    length: r.get::<_, i64>(2)? as u64,
                    offset: r.get::<_, Option<i64>>(3)?.map(|o| o as u64),
                })
            })
            .optional()
    }

    /// Queues a variants job for every present book with a page that lacks
    /// variants made with `version` and may need some: wider than
    /// `min_width`, or of unknown size. Returns how many were queued.
    pub fn queue_missing_variants(&self, version: i64, min_width: u32) -> Result<usize, Error> {
        self.conn.execute(
            "INSERT INTO jobs (kind, path)
             SELECT 'variants', books.path FROM books
             JOIN pages ON pages.book_id = books.id
             LEFT JOIN variant_pages v ON v.hash = pages.hash AND v.version = ?1
             WHERE books.gone IS NULL AND v.hash IS NULL
               AND (pages.width IS NULL OR pages.width > ?2)
             GROUP BY books.id ORDER BY books.id
             ON CONFLICT (kind, path) DO NOTHING",
            params![version, min_width],
        )
    }

    /// The pages of the present book at `path` that lack variants made with
    /// `version` and may need some (see [`Catalog::queue_missing_variants`]),
    /// each page content once, in reading order.
    pub fn pages_lacking_variants(
        &self,
        path: &str,
        version: i64,
        min_width: u32,
    ) -> Result<Vec<(blake3::Hash, PageLocation)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT pages.hash, COALESCE(pages.file, books.path), pages.format, pages.length,
                    pages.offset, MIN(pages.number) AS first
             FROM books JOIN pages ON pages.book_id = books.id
             LEFT JOIN variant_pages v ON v.hash = pages.hash AND v.version = ?2
             WHERE books.path = ?1 AND books.gone IS NULL AND v.hash IS NULL
               AND (pages.width IS NULL OR pages.width > ?3)
             GROUP BY pages.hash ORDER BY first",
        )?;
        let rows = stmt.query_map(params![path, version, min_width], |r| {
            Ok((
                hash_from_row(r, 0)?,
                PageLocation {
                    book_path: r.get(1)?,
                    format: r.get(2)?,
                    length: r.get::<_, i64>(3)? as u64,
                    offset: r.get::<_, Option<i64>>(4)?.map(|o| o as u64),
                },
            ))
        })?;
        rows.collect()
    }

    /// Records a page's variants (`(width, height, format, length)`; their
    /// blobs already written), made with `version`, replacing any it had.
    /// `error` says why a page has none.
    pub fn store_variants(
        &mut self,
        hash: &blake3::Hash,
        version: i64,
        variants: &[(u32, u32, &str, u64)],
        error: Option<&str>,
    ) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM variants WHERE hash = ?1", [hash.as_bytes()])?;
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO variants (hash, width, format, height, length)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for &(width, height, format, length) in variants {
                insert.execute(params![
                    hash.as_bytes(),
                    width,
                    format,
                    height,
                    length as i64
                ])?;
            }
        }
        tx.execute(
            "INSERT INTO variant_pages (hash, version, error) VALUES (?1, ?2, ?3)
             ON CONFLICT (hash) DO UPDATE SET version = excluded.version, error = excluded.error",
            params![hash.as_bytes(), version, error],
        )?;
        tx.commit()
    }

    /// How many page contents have their variants made with `version`.
    pub fn variant_page_count(&self, version: i64) -> Result<i64, Error> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM variant_pages WHERE version = ?1",
            [version],
            |r| r.get(0),
        )
    }

    /// One page of a book by its 0-based number.
    pub fn page(&self, book_id: i64, number: i64) -> Result<Option<Page>, Error> {
        self.conn
            .prepare_cached(
                "SELECT number, hash, format, length, offset, file, width, height FROM pages
                 WHERE book_id = ?1 AND number = ?2",
            )?
            .query_row(params![book_id, number], |r| {
                Ok(Page {
                    number: r.get(0)?,
                    hash: hash_from_row(r, 1)?,
                    format: r.get(2)?,
                    length: r.get::<_, i64>(3)? as u64,
                    offset: r.get::<_, Option<i64>>(4)?.map(|o| o as u64),
                    file: r.get(5)?,
                    size: match (r.get::<_, Option<u32>>(6)?, r.get::<_, Option<u32>>(7)?) {
                        (Some(width), Some(height)) => Some(Size { width, height }),
                        _ => None,
                    },
                })
            })
            .optional()
    }

    /// One page's variants made with `version`, as `(width, format)`.
    pub fn page_variants(
        &self,
        hash: &blake3::Hash,
        version: i64,
    ) -> Result<Vec<(u32, String)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT variants.width, variants.format FROM variants
             JOIN variant_pages vp ON vp.hash = variants.hash AND vp.version = ?2
             WHERE variants.hash = ?1 ORDER BY variants.width, variants.format",
        )?;
        let rows = stmt.query_map(params![hash.as_bytes(), version], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?;
        rows.collect()
    }

    /// A book's own reading direction (`"rtl"`, `"ltr"`), if its metadata
    /// gives one.
    pub fn book_direction(&self, book_id: i64) -> Result<Option<String>, Error> {
        Ok(self
            .conn
            .prepare_cached("SELECT direction FROM books WHERE id = ?1")?
            .query_row([book_id], |r| r.get(0))
            .optional()?
            .flatten())
    }

    /// A book's language as its metadata gives it (`''` if it does not).
    pub fn book_language(&self, book_id: i64) -> Result<String, Error> {
        Ok(self
            .conn
            .prepare_cached("SELECT language FROM books WHERE id = ?1")?
            .query_row([book_id], |r| r.get(0))
            .optional()?
            .unwrap_or_default())
    }

    /// The variants of every page of a book made with `version`, as
    /// `(page number, width, format)`, by page then width.
    pub fn book_variants(
        &self,
        book_id: i64,
        version: i64,
    ) -> Result<Vec<(i64, u32, String)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT pages.number, variants.width, variants.format FROM pages
             JOIN variants ON variants.hash = pages.hash
             JOIN variant_pages vp ON vp.hash = pages.hash AND vp.version = ?2
             WHERE pages.book_id = ?1
             ORDER BY pages.number, variants.width, variants.format",
        )?;
        let rows = stmt.query_map(params![book_id, version], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        rows.collect()
    }

    /// The variants of a page, as `(width, height, format)`, narrowest first.
    pub fn variants_of(&self, hash: &blake3::Hash) -> Result<Vec<(u32, u32, String)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT width, height, format FROM variants WHERE hash = ?1 ORDER BY width, format",
        )?;
        let rows = stmt.query_map([hash.as_bytes()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect()
    }

    /// Whether a page of a present book has this variant, made with `version`.
    pub fn has_variant(
        &self,
        hash: &blake3::Hash,
        width: u32,
        version: i64,
        format: &str,
    ) -> Result<bool, Error> {
        self.conn
            .prepare_cached(
                "SELECT 1 FROM variants
                 JOIN variant_pages v ON v.hash = variants.hash AND v.version = ?3
                 WHERE variants.hash = ?1 AND variants.width = ?2 AND variants.format = ?4
                   AND EXISTS (SELECT 1 FROM pages JOIN books ON books.id = pages.book_id
                               WHERE pages.hash = ?1 AND books.gone IS NULL)",
            )?
            .query_row(params![hash.as_bytes(), width, version, format], |_| Ok(()))
            .optional()
            .map(|r| r.is_some())
    }
}

/// What a job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// Index a book file: hash and locate its pages.
    Index,
    /// Make the variants of a book's pages that lack them.
    Variants,
}

impl JobKind {
    fn as_str(self) -> &'static str {
        match self {
            JobKind::Index => "index",
            JobKind::Variants => "variants",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "index" => Some(JobKind::Index),
            "variants" => Some(JobKind::Variants),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub id: i64,
    pub kind: JobKind,
    pub path: String,
    /// How many times it has been claimed, this time included.
    pub attempts: i64,
}

/// The id of the series at `path`, made if needed. `moving` is the book
/// being placed and the series it was in: if no other present book is left
/// there, that series moves to `path` with its id (a series folder renamed or
/// moved outside Koma-1), unless `path` already has a series.
fn place_in_series(
    tx: &rusqlite::Transaction<'_>,
    path: &str,
    name: &str,
    moving: Option<(i64, i64)>,
) -> Result<i64, Error> {
    let existing: Option<i64> = tx
        .query_row("SELECT id FROM series WHERE path = ?1", [path], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(id) = existing {
        let old: String =
            tx.query_row("SELECT name FROM series WHERE id = ?1", [id], |r| r.get(0))?;
        tx.execute(
            "UPDATE series SET name = ?2 WHERE id = ?1",
            params![id, series_name(&old, name)],
        )?;
        return Ok(id);
    }
    if let Some((book, series)) = moving {
        let others: i64 = tx.query_row(
            "SELECT COUNT(*) FROM books WHERE series_id = ?1 AND id != ?2 AND gone IS NULL",
            params![series, book],
            |r| r.get(0),
        )?;
        if others == 0 {
            tx.execute(
                "UPDATE series SET path = ?2, name = ?3 WHERE id = ?1",
                params![series, path, name],
            )?;
            return Ok(series);
        }
    }
    tx.execute(
        "INSERT INTO series (path, name) VALUES (?1, ?2)",
        params![path, name],
    )?;
    Ok(tx.last_insert_rowid())
}

/// The name a series shows when its books name it differently: books loose
/// in the root group by their name without case (`starfall academy v03.cbz`
/// joins `Starfall Academy - Ch. 01.cbz`). The best-capitalised spelling wins,
/// then the first in order, so the name doesn't depend on scan order.
fn series_name<'a>(a: &'a str, b: &'a str) -> &'a str {
    let capitals = |s: &str| s.chars().filter(|c| c.is_uppercase()).count();
    match capitals(a).cmp(&capitals(b)).then_with(|| b.cmp(a)) {
        std::cmp::Ordering::Less => b,
        _ => a,
    }
}

fn last_part(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn prune_series(tx: &rusqlite::Transaction<'_>) -> Result<(), Error> {
    tx.execute(
        "DELETE FROM series WHERE id NOT IN (SELECT series_id FROM books)",
        [],
    )?;
    Ok(())
}

fn hash_from_row(r: &rusqlite::Row<'_>, i: usize) -> Result<blake3::Hash, Error> {
    let bytes: [u8; 32] = r.get(i)?;
    Ok(blake3::Hash::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::series_name;

    #[test]
    fn series_name_does_not_depend_on_order() {
        for (a, b) in [
            ("starfall academy", "Starfall Academy"),
            ("Starfall academy", "starfall Academy"),
            ("Starfall", "Starfall"),
        ] {
            assert_eq!(series_name(a, b), series_name(b, a), "{a} / {b}");
        }
        assert_eq!(
            series_name("starfall academy", "Starfall Academy"),
            "Starfall Academy"
        );
    }
}
