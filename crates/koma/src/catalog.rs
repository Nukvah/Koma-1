//! The catalogue: series, books and pages in SQLite.
//!
//! `koma worker` writes it; `koma serve` only reads it. Names and paths sort
//! with the `natsort` collation: [`crate::natsort::compare_paths`].
//!
//! It also holds what the scan needs to stay incremental (folders and their
//! modification times) and the worker's job queue.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

pub use rusqlite::Error;

const SCHEMA_VERSION: i64 = 3;

/// A job claimed this many times without finishing is given up: the worker
/// died on it each time (crash, out of memory), so it would only die again.
pub const MAX_ATTEMPTS: i64 = 3;

const SCHEMA: &str = "
CREATE TABLE series (
    id    INTEGER PRIMARY KEY,
    path  TEXT NOT NULL UNIQUE,      -- folder relative to the library (or the file, for a one-shot)
    name  TEXT NOT NULL
);
CREATE TABLE books (
    id         INTEGER PRIMARY KEY,
    series_id  INTEGER NOT NULL REFERENCES series(id),
    path       TEXT NOT NULL UNIQUE, -- file (or photo folder) relative to the library
    folder     TEXT NOT NULL,        -- its folder relative to the library ('' is the root);
                                     -- a photo folder's own path
    name       TEXT NOT NULL,
    size       INTEGER NOT NULL,     -- with mtime: skip unchanged files on rescan
    mtime      INTEGER NOT NULL,
    page_count INTEGER NOT NULL
);
CREATE INDEX books_series ON books(series_id);
CREATE INDEX books_folder ON books(folder);
CREATE TABLE pages (
    book_id  INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    number   INTEGER NOT NULL,       -- 0-based reading order
    hash     BLOB NOT NULL,          -- BLAKE3 of the page bytes
    format   TEXT NOT NULL,          -- file extension: jpg, png, webp...
    length   INTEGER NOT NULL,
    offset   INTEGER,                -- byte offset in the book file; NULL: in the blob store
    file     TEXT,                   -- the file holding the page if not the book's (photo folders)
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
";

/// Tables of earlier schema versions, dropped when upgrading: until accounts
/// exist the catalogue holds nothing that the next scan cannot rebuild.
const DROP_OLD: &str = "
DROP TABLE IF EXISTS pages;
DROP TABLE IF EXISTS books;
DROP TABLE IF EXISTS series;
DROP TABLE IF EXISTS scan_errors;
DROP TABLE IF EXISTS folders;
DROP TABLE IF EXISTS jobs;
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
}

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
            0..SCHEMA_VERSION => {
                let drop = if version > 0 { DROP_OLD } else { "" };
                self.conn.execute_batch(&format!(
                    "BEGIN; {drop} {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
                Ok(())
            }
            SCHEMA_VERSION => Ok(()),
            _ => Err(Error::InvalidQuery),
        }
    }

    /// Size and modification time recorded for a book, if it is known.
    pub fn book_stamp(&self, path: &str) -> Result<Option<Stamp>, Error> {
        self.conn
            .query_row(
                "SELECT size, mtime FROM books WHERE path = ?1",
                [path],
                |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
            )
            .optional()
    }

    /// Stores a book and its pages, replacing what was known about that file.
    pub fn replace_book(&mut self, book: &NewBook<'_>) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO series (path, name) VALUES (?1, ?2)
             ON CONFLICT (path) DO UPDATE SET name = excluded.name",
            params![book.series_path, book.series_name],
        )?;
        let series_id: i64 = tx.query_row(
            "SELECT id FROM series WHERE path = ?1",
            [book.series_path],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM books WHERE path = ?1", [book.path])?;
        tx.execute(
            "INSERT INTO books (series_id, path, folder, name, size, mtime, page_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                series_id,
                book.path,
                book.folder,
                book.name,
                book.size as i64,
                book.mtime,
                book.pages.len() as i64
            ],
        )?;
        let book_id = tx.last_insert_rowid();
        {
            let mut insert = tx.prepare(
                "INSERT INTO pages (book_id, number, hash, format, length, offset, file)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
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
                ])?;
            }
        }
        tx.execute("DELETE FROM scan_errors WHERE path = ?1", [book.path])?;
        // The series may have been left empty by a book that changed series.
        prune_series(&tx)?;
        tx.commit()
    }

    /// Forgets a book (its file is gone or no longer readable), and its series
    /// if that is left empty. Returns whether the book was known.
    pub fn remove_book(&mut self, path: &str) -> Result<bool, Error> {
        let tx = self.conn.transaction()?;
        let removed = tx.execute("DELETE FROM books WHERE path = ?1", [path])? > 0;
        prune_series(&tx)?;
        tx.commit()?;
        Ok(removed)
    }

    /// Forgets a folder that is gone: everything known under it, and series
    /// left empty. Returns how many books were removed.
    pub fn remove_tree(&mut self, folder: &str) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        // Inside `folder`: the folder itself, or anything under `folder/`
        // (paths that sort from "folder/" up to, not including, "folder0").
        let under = "(?1 = col OR (col >= ?1 || '/' AND col < ?1 || '0'))";
        let removed = tx.execute(
            &format!("DELETE FROM books WHERE {}", under.replace("col", "folder")),
            [folder],
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
        let mut stmt = self
            .conn
            .prepare_cached("SELECT path, size, mtime FROM books WHERE folder = ?1")?;
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
    pub fn claim_job(&self) -> Result<Option<Job>, Error> {
        self.conn
            .prepare_cached(
                "UPDATE jobs SET running = 1, attempts = attempts + 1
                 WHERE id = (SELECT id FROM jobs WHERE running = 0 ORDER BY id LIMIT 1)
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

    /// All series, in natural order of their names.
    pub fn series(&self) -> Result<Vec<Series>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path, name FROM series ORDER BY name COLLATE natsort, path COLLATE natsort",
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
             FROM series JOIN books ON books.series_id = series.id
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
                "SELECT id, series_id, path, name, page_count FROM books WHERE id = ?1",
            )?
            .query_row([id], |r| {
                Ok(Book {
                    id: r.get(0)?,
                    series_id: r.get(1)?,
                    path: r.get(2)?,
                    name: r.get(3)?,
                    page_count: r.get(4)?,
                })
            })
            .optional()
    }

    /// The books of a series, in natural order of their file names.
    pub fn books(&self, series_id: i64) -> Result<Vec<Book>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, series_id, path, name, page_count FROM books
             WHERE series_id = ?1 ORDER BY path COLLATE natsort",
        )?;
        let rows = stmt.query_map([series_id], |r| {
            Ok(Book {
                id: r.get(0)?,
                series_id: r.get(1)?,
                path: r.get(2)?,
                name: r.get(3)?,
                page_count: r.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// The pages of a book, in reading order.
    pub fn pages(&self, book_id: i64) -> Result<Vec<Page>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT number, hash, format, length, offset, file FROM pages
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
            })
        })?;
        rows.collect()
    }

    /// Where to read a page with this hash, preferring one inside an archive.
    pub fn page_by_hash(&self, hash: &blake3::Hash) -> Result<Option<PageLocation>, Error> {
        self.conn
            .prepare_cached(
                "SELECT COALESCE(pages.file, books.path), pages.format, pages.length, pages.offset
                 FROM pages JOIN books ON books.id = pages.book_id
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
}

/// What a job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    /// Index a book file: hash and locate its pages.
    Index,
}

impl JobKind {
    fn as_str(self) -> &'static str {
        match self {
            JobKind::Index => "index",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "index" => Some(JobKind::Index),
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
