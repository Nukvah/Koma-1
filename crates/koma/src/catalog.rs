//! The catalogue: series, books and pages in SQLite.
//!
//! `koma worker` writes it; `koma serve` only reads it. Names and paths sort
//! with the `natsort` collation: [`crate::natsort::compare_paths`].

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

pub use rusqlite::Error;

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE series (
    id    INTEGER PRIMARY KEY,
    path  TEXT NOT NULL UNIQUE,      -- folder relative to the library (or the file, for a one-shot)
    name  TEXT NOT NULL
);
CREATE TABLE books (
    id         INTEGER PRIMARY KEY,
    series_id  INTEGER NOT NULL REFERENCES series(id),
    path       TEXT NOT NULL UNIQUE, -- file relative to the library
    name       TEXT NOT NULL,
    size       INTEGER NOT NULL,     -- with mtime: skip unchanged files on rescan
    mtime      INTEGER NOT NULL,
    page_count INTEGER NOT NULL
);
CREATE INDEX books_series ON books(series_id);
CREATE TABLE pages (
    book_id  INTEGER NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    number   INTEGER NOT NULL,       -- 0-based reading order
    hash     BLOB NOT NULL,          -- BLAKE3 of the page bytes
    format   TEXT NOT NULL,          -- file extension: jpg, png, webp...
    length   INTEGER NOT NULL,
    offset   INTEGER,                -- byte offset in the book file; NULL: in the blob store
    PRIMARY KEY (book_id, number)
) WITHOUT ROWID;
CREATE INDEX pages_hash ON pages(hash);
CREATE TABLE scan_errors (
    path   TEXT PRIMARY KEY,
    error  TEXT NOT NULL
);
";

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
    /// Offset in the book file, or `None` when the page is in the blob store.
    pub offset: Option<u64>,
}

/// Where a page's bytes are, found by hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageLocation {
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
            0 => {
                self.conn.execute_batch(&format!(
                    "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                ))?;
                Ok(())
            }
            SCHEMA_VERSION => Ok(()),
            _ => Err(Error::InvalidQuery),
        }
    }

    /// Size and modification time recorded for a book, if it is known.
    pub fn book_stamp(&self, path: &str) -> Result<Option<(u64, i64)>, Error> {
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
            "INSERT INTO books (series_id, path, name, size, mtime, page_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                series_id,
                book.path,
                book.name,
                book.size as i64,
                book.mtime,
                book.pages.len() as i64
            ],
        )?;
        let book_id = tx.last_insert_rowid();
        {
            let mut insert = tx.prepare(
                "INSERT INTO pages (book_id, number, hash, format, length, offset)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )?;
            for page in book.pages {
                insert.execute(params![
                    book_id,
                    page.number,
                    page.hash.as_bytes(),
                    page.format,
                    page.length as i64,
                    page.offset.map(|o| o as i64),
                ])?;
            }
        }
        tx.execute("DELETE FROM scan_errors WHERE path = ?1", [book.path])?;
        tx.commit()
    }

    /// Forgets books whose files are gone, and series left empty. Returns how
    /// many books were removed.
    pub fn retain_books(&mut self, keep: &dyn Fn(&str) -> bool) -> Result<usize, Error> {
        let tx = self.conn.transaction()?;
        let gone: Vec<String> = {
            let mut stmt = tx.prepare("SELECT path FROM books")?;
            let paths = stmt.query_map([], |r| r.get::<_, String>(0))?;
            paths
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter(|p| !keep(p))
                .collect()
        };
        for path in &gone {
            tx.execute("DELETE FROM books WHERE path = ?1", [path])?;
        }
        tx.execute(
            "DELETE FROM series WHERE id NOT IN (SELECT series_id FROM books)",
            [],
        )?;
        tx.commit()?;
        Ok(gone.len())
    }

    pub fn clear_errors(&self) -> Result<(), Error> {
        self.conn.execute_batch("DELETE FROM scan_errors")
    }

    pub fn record_error(&self, path: &str, error: &str) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO scan_errors (path, error) VALUES (?1, ?2)
             ON CONFLICT (path) DO UPDATE SET error = excluded.error",
            params![path, error],
        )?;
        Ok(())
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
            "SELECT number, hash, format, length, offset FROM pages
             WHERE book_id = ?1 ORDER BY number",
        )?;
        let rows = stmt.query_map([book_id], |r| {
            Ok(Page {
                number: r.get(0)?,
                hash: hash_from_row(r, 1)?,
                format: r.get(2)?,
                length: r.get::<_, i64>(3)? as u64,
                offset: r.get::<_, Option<i64>>(4)?.map(|o| o as u64),
            })
        })?;
        rows.collect()
    }

    /// Where to read a page with this hash, preferring one inside an archive.
    pub fn page_by_hash(&self, hash: &blake3::Hash) -> Result<Option<PageLocation>, Error> {
        self.conn
            .prepare_cached(
                "SELECT books.path, pages.format, pages.length, pages.offset
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

fn hash_from_row(r: &rusqlite::Row<'_>, i: usize) -> Result<blake3::Hash, Error> {
    let bytes: [u8; 32] = r.get(i)?;
    Ok(blake3::Hash::from_bytes(bytes))
}
