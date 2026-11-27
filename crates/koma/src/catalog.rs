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

const SCHEMA_VERSION: i64 = 8;

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
    library TEXT GENERATED ALWAYS AS ({LIBRARY_OF_PATH}) VIRTUAL
);
CREATE INDEX series_library ON series(library);
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

/// Tables of earlier schema versions, dropped when upgrading: until accounts
/// exist the catalogue holds nothing that the next scan cannot rebuild.
const DROP_OLD: &str = "
DROP TABLE IF EXISTS pages;
DROP TABLE IF EXISTS book_terms;
DROP TABLE IF EXISTS books;
DROP TABLE IF EXISTS series;
DROP TABLE IF EXISTS scan_errors;
DROP TABLE IF EXISTS folders;
DROP TABLE IF EXISTS jobs;
DROP TABLE IF EXISTS variants;
DROP TABLE IF EXISTS variant_pages;
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
    pub details: &'a Details,
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
            7 => {
                self.conn.execute_batch(&format!(
                    "BEGIN; {} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;",
                    migrate_7_to_8()
                ))?;
                Ok(())
            }
            0..SCHEMA_VERSION => {
                // Before 7, nothing was kept that a scan cannot rebuild.
                let drop = if version > 0 { DROP_OLD } else { "" };
                let schema = SCHEMA.replace("{LIBRARY_OF_PATH}", LIBRARY_OF_PATH);
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
                book.pages.len() as i64,
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
                "SELECT id, series_id, path, name, page_count, added FROM books
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
                })
            })
            .optional()
    }

    /// The books of a series, in natural order of their file names.
    pub fn books(&self, series_id: i64) -> Result<Vec<Book>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, series_id, path, name, page_count, added FROM books
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
