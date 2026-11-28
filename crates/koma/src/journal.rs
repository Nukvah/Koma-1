//! The catalogue's side of organising the library (see `crate::organise`):
//! books and series moved keeping their ids, the trash, details edited in
//! the UI, the state of each change, and the append-only journal of what
//! each change did to files, which undo reads backwards.
//!
//! Only the worker calls the writing functions here, after it has changed
//! the files: the catalogue follows the disk, never the other way round.

use rusqlite::{Connection, OptionalExtension, params};

use crate::catalog::{Catalog, Error, place_in_series, prune_series};

/// Fields of a book that can be edited, as stored in `edits`.
pub const FIELDS: [&str; 11] = [
    "title",
    "number",
    "kind",
    "summary",
    "year",
    "direction",
    "cover",
    "authors",
    "publishers",
    "genres",
    "tags",
];

/// The `book_terms` kind of a list field.
fn term_kind(field: &str) -> Option<&'static str> {
    Some(match field {
        "authors" => "author",
        "publishers" => "publisher",
        "genres" => "genre",
        "tags" => "tag",
        _ => return None,
    })
}

/// Writes a book's edited details over what its files said. Called after
/// every (re)index and every edit.
pub(crate) fn apply_edits(conn: &Connection, book: i64) -> Result<(), Error> {
    let mut stmt = conn.prepare_cached("SELECT field, value FROM edits WHERE book_id = ?1")?;
    let edits: Vec<(String, String)> = stmt
        .query_map([book], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (field, value) in edits {
        let v: serde_json::Value = serde_json::from_str(&value).unwrap_or_default();
        if let Some(kind) = term_kind(&field) {
            conn.execute(
                "DELETE FROM book_terms WHERE book_id = ?1 AND kind = ?2",
                params![book, kind],
            )?;
            let names: Vec<String> = v
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            for (pos, name) in names.iter().enumerate() {
                conn.execute(
                    "INSERT INTO book_terms (book_id, kind, name, pos) VALUES (?1, ?2, ?3, ?4)",
                    params![book, kind, name, pos as i64],
                )?;
            }
            continue;
        }
        let column = match field.as_str() {
            "title" | "summary" | "number" | "kind" | "year" | "direction" => field.as_str(),
            "cover" => "cover_page",
            _ => continue,
        };
        let value: rusqlite::types::Value = match v {
            serde_json::Value::String(s) => s.into(),
            serde_json::Value::Number(n) if n.is_i64() => n.as_i64().unwrap_or_default().into(),
            serde_json::Value::Number(n) => n.as_f64().unwrap_or_default().into(),
            _ if matches!(column, "title" | "summary") => String::new().into(),
            _ => rusqlite::types::Value::Null,
        };
        conn.execute(
            &format!("UPDATE books SET {column} = ?2 WHERE id = ?1"),
            params![book, value],
        )?;
    }
    Ok(())
}

/// A change's progress, as the worker records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpState {
    pub id: i64,
    /// `running`, `paused`, `done`, `failed` or `cancelled`.
    pub state: String,
    pub done: i64,
    pub total: i64,
    pub message: String,
    pub started: i64,
    pub finished: Option<i64>,
}

/// One line of the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub seq: i64,
    pub op: i64,
    /// `move`, `folder`, `trash`, `restore`, `erase`, `edit`, `write`,
    /// `backup`, `mkdir`.
    pub action: String,
    pub book: Option<i64>,
    pub series: Option<i64>,
    pub from: String,
    pub to: String,
    /// Of what is at `to` afterwards, for undo to check it is unchanged.
    pub size: Option<i64>,
    pub mtime: Option<i64>,
    pub data: Option<String>,
    pub at: i64,
}

/// Something in the trash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashItem {
    pub id: i64,
    pub op: i64,
    /// `book` or `series`.
    pub kind: String,
    pub name: String,
    pub series: i64,
    pub from: String,
    pub to: String,
    pub size: i64,
    pub by: String,
    pub at: i64,
}

/// Rewrites `path` (relative to the library) when it is `old` or under
/// it: `old/x` → `new/x`.
pub fn rebase(path: &str, old: &str, new: &str) -> Option<String> {
    if path == old {
        Some(new.to_owned())
    } else {
        path.strip_prefix(old)
            .and_then(|rest| rest.strip_prefix('/'))
            .map(|rest| format!("{new}/{rest}"))
    }
}

/// SQL true when `col` is `?1` or under `?1/`.
const UNDER: &str = "(col = ?1 OR (col >= ?1 || '/' AND col < ?1 || '0'))";

fn under(col: &str) -> String {
    UNDER.replace("col", col)
}

impl Catalog {
    // State of changes.

    pub fn op_state(&self, id: i64) -> Result<Option<OpState>, Error> {
        self.conn
            .query_row(
                "SELECT id, state, done, total, message, started, finished FROM ops WHERE id = ?1",
                [id],
                op_row,
            )
            .optional()
    }

    /// The most recent changes' states, newest first.
    pub fn op_states(&self, limit: usize) -> Result<Vec<OpState>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, state, done, total, message, started, finished FROM ops
             ORDER BY id DESC LIMIT ?1",
        )?;
        stmt.query_map([limit as i64], op_row)?.collect()
    }

    /// The highest change id the worker has taken up; 0 if none.
    pub fn last_op(&self) -> Result<i64, Error> {
        self.conn.query_row(
            "SELECT coalesce(max(id), 0) FROM ops WHERE id > 0",
            [],
            |r| r.get(0),
        )
    }

    /// Starts one of the worker's own changes (the cosplay tidy): a negative
    /// id, with its kind and title kept here since accounts.db has no row.
    pub fn new_worker_op(&self, kind: &str, title: &str) -> Result<i64, Error> {
        let id: i64 = self.conn.query_row(
            "SELECT min(coalesce(min(id), 0), 0) - 1 FROM ops",
            [],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO ops (id, state, done, total, message, started, kind, title)
             VALUES (?1, 'running', 0, 0, '', ?2, ?3, ?4)",
            params![id, crate::accounts::now(), kind, title],
        )?;
        Ok(id)
    }

    /// The worker's own changes, newest first: `(state, kind, title)`.
    pub fn worker_ops(&self, limit: usize) -> Result<Vec<(OpState, String, String)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, state, done, total, message, started, finished, kind, title FROM ops
             WHERE id < 0 ORDER BY id LIMIT ?1",
        )?;
        stmt.query_map([limit as i64], |r| {
            Ok((
                op_row(r)?,
                r.get::<_, Option<String>>(7)?.unwrap_or_default(),
                r.get::<_, Option<String>>(8)?.unwrap_or_default(),
            ))
        })?
        .collect()
    }

    /// The kind of one of the worker's own changes.
    pub fn worker_op_kind(&self, id: i64) -> Result<Option<(String, String)>, Error> {
        self.conn
            .query_row(
                "SELECT coalesce(kind, ''), coalesce(title, '') FROM ops WHERE id = ?1 AND id < 0",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// Names the tidy leaves as they are.
    pub fn kept_names(&self) -> Result<Vec<String>, Error> {
        let mut stmt = self.conn.prepare_cached("SELECT path FROM name_keep")?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    pub fn keep_name(&self, path: &str) -> Result<(), Error> {
        self.conn
            .execute("INSERT OR IGNORE INTO name_keep (path) VALUES (?1)", [path])?;
        Ok(())
    }

    /// Changes left running or paused (a restart, or a pause), oldest first.
    pub fn unfinished_ops(&self) -> Result<Vec<i64>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id FROM ops WHERE state IN ('running', 'paused') AND id > 0 ORDER BY id",
        )?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    pub fn set_op(
        &self,
        id: i64,
        state: &str,
        done: i64,
        total: i64,
        message: &str,
    ) -> Result<(), Error> {
        let now = crate::accounts::now();
        let finished = matches!(state, "done" | "failed" | "cancelled").then_some(now);
        self.conn.execute(
            "INSERT INTO ops (id, state, done, total, message, started, finished)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (id) DO UPDATE SET state = ?2, done = ?3, total = ?4, message = ?5,
               finished = ?7",
            params![id, state, done, total, message, now, finished],
        )?;
        Ok(())
    }

    // The journal.

    pub fn journal_add(&self, e: &Entry) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO journal (op, action, book, series, from_path, to_path, size, mtime, data, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                e.op,
                e.action,
                e.book,
                e.series,
                e.from,
                e.to,
                e.size,
                e.mtime,
                e.data,
                crate::accounts::now()
            ],
        )?;
        Ok(())
    }

    /// A change's lines, in the order they were written.
    pub fn journal_of(&self, op: i64) -> Result<Vec<Entry>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{ENTRY} WHERE op = ?1 ORDER BY seq"))?;
        stmt.query_map([op], entry_row)?.collect()
    }

    /// The last lines of the journal, newest first.
    pub fn journal_recent(&self, limit: usize) -> Result<Vec<Entry>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{ENTRY} ORDER BY seq DESC LIMIT ?1"))?;
        stmt.query_map([limit as i64], entry_row)?.collect()
    }

    // Moving.

    /// A book whose file (or photo folder) the worker moved to `to`: it
    /// keeps its id, progress, edits and date added, and joins the series
    /// of its new folder.
    pub fn move_book(&mut self, id: i64, to: &str, photos: bool) -> Result<(), Error> {
        let (folder, name, (series_path, series_name)) = crate::scan::place_of(to, photos);
        self.relink(
            id,
            &crate::catalog::Place {
                path: to,
                folder: &folder,
                name: &name,
                series_path: &series_path,
                series_name: &series_name,
            },
        )?;
        self.forget_folders(crate::scan::folder_of_path(to))
    }

    /// A folder the worker moved from `old` to `new` (a series renamed or
    /// moved, or to and from the trash): every series and book under it
    /// follows, keeping their ids.
    pub fn move_folder(&mut self, old: &str, new: &str) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        for (table, cols) in [
            ("series", &["path"][..]),
            ("books", &["path", "folder"][..]),
        ] {
            let mut rows: Vec<(i64, Vec<String>)> = Vec::new();
            {
                let mut stmt = tx.prepare(&format!(
                    "SELECT id, {} FROM {table} WHERE {}",
                    cols.join(", "),
                    under(cols[0])
                ))?;
                let found = stmt.query_map([old], |r| {
                    let mut v = Vec::new();
                    for i in 0..cols.len() {
                        v.push(r.get::<_, String>(i + 1)?);
                    }
                    Ok((r.get::<_, i64>(0)?, v))
                })?;
                for f in found {
                    rows.push(f?);
                }
            }
            for (id, values) in rows {
                let new_values: Vec<String> = values
                    .iter()
                    .map(|v| rebase(v, old, new).unwrap_or_else(|| v.clone()))
                    .collect();
                let set: Vec<String> = cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{c} = ?{}", i + 2))
                    .collect();
                let mut args: Vec<rusqlite::types::Value> = vec![id.into()];
                args.extend(new_values.into_iter().map(Into::into));
                tx.execute(
                    &format!("UPDATE {table} SET {} WHERE id = ?1", set.join(", ")),
                    rusqlite::params_from_iter(args),
                )?;
            }
        }
        // A series folder renamed: its name follows the folder's.
        let name = new.rsplit('/').next().unwrap_or(new);
        tx.execute(
            "UPDATE series SET name = ?2 WHERE path = ?1",
            params![new, name],
        )?;
        tx.execute(
            &format!("DELETE FROM scan_errors WHERE {}", under("path")),
            [old],
        )?;
        tx.commit()?;
        self.forget_folders(old)?;
        self.forget_folders(crate::scan::folder_of_path(old))?;
        self.forget_folders(crate::scan::folder_of_path(new))
    }

    /// Makes the next scan read `folder` (and what is under it) again in
    /// full, after the worker changed it.
    fn forget_folders(&self, folder: &str) -> Result<(), Error> {
        self.conn.execute(
            &format!("UPDATE folders SET mtime = NULL WHERE {}", under("path")),
            [folder],
        )?;
        self.conn
            .execute("UPDATE folders SET mtime = NULL WHERE path = ''", [])?;
        Ok(())
    }

    /// The present book at `path`, if any: `(id, photos)`.
    pub fn book_by_path(&self, path: &str) -> Result<Option<(i64, bool)>, Error> {
        self.conn
            .query_row(
                "SELECT id, folder = path FROM books WHERE path = ?1 AND gone IS NULL",
                [path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    /// A series' present books' paths: `(id, path, photos)`.
    pub fn book_paths(&self, series: i64) -> Result<Vec<(i64, String, bool)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path, folder = path FROM books WHERE series_id = ?1 AND gone IS NULL
             ORDER BY path COLLATE natsort",
        )?;
        stmt.query_map([series], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    }

    // The trash.

    /// Records that the worker moved `item.from` to `item.to` in the trash:
    /// the books there leave the library (gone) but keep their ids and are
    /// not forgotten while the trash keeps them.
    pub fn trash_add(&mut self, item: &TrashItem, books: &[i64]) -> Result<i64, Error> {
        self.conn.execute(
            "INSERT INTO trash (op, kind, name, series, from_path, to_path, size, by, at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                item.op,
                item.kind,
                item.name,
                item.series,
                item.from,
                item.to,
                item.size,
                item.by,
                item.at
            ],
        )?;
        let id = self.conn.last_insert_rowid();
        if item.kind == "series" {
            self.move_folder(&item.from, &item.to)?;
        } else {
            for &b in books {
                let path: String =
                    self.conn
                        .query_row("SELECT path FROM books WHERE id = ?1", [b], |r| r.get(0))?;
                let to = rebase(&path, &item.from, &item.to).unwrap_or(item.to.clone());
                self.conn.execute(
                    "UPDATE books SET path = ?2, folder = ?3 WHERE id = ?1",
                    params![b, to, crate::scan::folder_of_path(&to)],
                )?;
            }
            self.forget_folders(crate::scan::folder_of_path(&item.from))?;
        }
        let now = crate::accounts::now();
        for &b in books {
            self.conn.execute(
                "UPDATE books SET gone = ?2, trash = ?3 WHERE id = ?1",
                params![b, now, id],
            )?;
        }
        Ok(id)
    }

    pub fn trash_items(&self) -> Result<Vec<TrashItem>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{TRASH} ORDER BY at DESC, id DESC"))?;
        stmt.query_map([], trash_row)?.collect()
    }

    pub fn trash_item(&self, id: i64) -> Result<Option<TrashItem>, Error> {
        self.conn
            .query_row(&format!("{TRASH} WHERE id = ?1"), [id], trash_row)
            .optional()
    }

    /// The trash item an op made for a book or series.
    pub fn trash_of_op(&self, op: i64) -> Result<Vec<TrashItem>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached(&format!("{TRASH} WHERE op = ?1"))?;
        stmt.query_map([op], trash_row)?.collect()
    }

    /// The books in a trash item: `(id, path, photos)`.
    pub fn trash_books(&self, item: i64) -> Result<Vec<(i64, String, bool)>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, path, folder = path FROM books WHERE trash = ?1 ORDER BY path",
        )?;
        stmt.query_map([item], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect()
    }

    /// The worker put a trash item back at `to`: its books are in the
    /// library again, with their ids.
    pub fn trash_restored(&mut self, item: &TrashItem, to: &str) -> Result<(), Error> {
        let books = self.trash_books(item.id)?;
        self.conn.execute(
            "UPDATE books SET gone = NULL, trash = NULL WHERE trash = ?1",
            [item.id],
        )?;
        if item.kind == "series" {
            self.move_folder(&item.to, to)?;
        } else {
            for (id, path, photos) in books {
                let target = rebase(&path, &item.to, to).unwrap_or(to.to_owned());
                self.move_book(id, &target, photos)?;
            }
        }
        self.conn
            .execute("DELETE FROM trash WHERE id = ?1", [item.id])?;
        Ok(())
    }

    /// The worker erased a trash item's files: its books are forgotten.
    pub fn trash_erased(&mut self, item: i64) -> Result<(), Error> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM books WHERE trash = ?1", [item])?;
        tx.execute("DELETE FROM trash WHERE id = ?1", [item])?;
        prune_series(&tx)?;
        tx.commit()
    }

    // Details.

    /// The details edited for a book, as `(field, JSON)`.
    pub fn edits(&self, book: i64) -> Result<Vec<(String, String)>, Error> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT field, value FROM edits WHERE book_id = ?1 ORDER BY field")?;
        stmt.query_map([book], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect()
    }

    /// Replaces a book's edits with `edits`, and writes them over its
    /// details. The previous ones are returned, for undo.
    pub fn set_edits(
        &mut self,
        book: i64,
        edits: &[(String, String)],
    ) -> Result<Vec<(String, String)>, Error> {
        let before = self.edits(book)?;
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM edits WHERE book_id = ?1", [book])?;
        for (field, value) in edits {
            if FIELDS.contains(&field.as_str()) {
                tx.execute(
                    "INSERT INTO edits (book_id, field, value) VALUES (?1, ?2, ?3)",
                    params![book, field, value],
                )?;
            }
        }
        apply_edits(&tx, book)?;
        tx.commit()?;
        Ok(before)
    }

    /// Ensures a series exists for `path`, as a new folder for books about
    /// to be moved there; returns its id.
    pub fn series_at(&mut self, path: &str) -> Result<Option<i64>, Error> {
        self.conn
            .query_row("SELECT id FROM series WHERE path = ?1", [path], |r| {
                r.get(0)
            })
            .optional()
    }

    /// The first folder of every present book's path (library candidates).
    pub fn book_tops(&self) -> Result<Vec<String>, Error> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT DISTINCT CASE WHEN instr(path, '/') > 0 THEN substr(path, 1, instr(path, '/') - 1)
             ELSE '' END FROM books WHERE gone IS NULL",
        )?;
        stmt.query_map([], |r| r.get(0))?.collect()
    }

    /// Present books under the folder `folder`: `(id, path, photos, series)`.
    pub fn books_under(&self, folder: &str) -> Result<Vec<(i64, String, bool, i64)>, Error> {
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT id, path, folder = path, series_id FROM books WHERE gone IS NULL AND {}
             AND path <> ?1 ORDER BY path COLLATE natsort",
            under("path")
        ))?;
        stmt.query_map([folder], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect()
    }

    /// Used by tests: makes sure a series row for `path` exists.
    pub fn ensure_series(&mut self, path: &str, name: &str) -> Result<i64, Error> {
        let tx = self.conn.transaction()?;
        let id = place_in_series(&tx, path, name, None)?;
        tx.commit()?;
        Ok(id)
    }
}

const ENTRY: &str =
    "SELECT seq, op, action, book, series, from_path, to_path, size, mtime, data, at FROM journal";

fn entry_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        seq: r.get(0)?,
        op: r.get(1)?,
        action: r.get(2)?,
        book: r.get(3)?,
        series: r.get(4)?,
        from: r.get(5)?,
        to: r.get(6)?,
        size: r.get(7)?,
        mtime: r.get(8)?,
        data: r.get(9)?,
        at: r.get(10)?,
    })
}

fn op_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<OpState> {
    Ok(OpState {
        id: r.get(0)?,
        state: r.get(1)?,
        done: r.get(2)?,
        total: r.get(3)?,
        message: r.get(4)?,
        started: r.get(5)?,
        finished: r.get(6)?,
    })
}

const TRASH: &str =
    "SELECT id, op, kind, name, series, from_path, to_path, size, by, at FROM trash";

fn trash_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<TrashItem> {
    Ok(TrashItem {
        id: r.get(0)?,
        op: r.get(1)?,
        kind: r.get(2)?,
        name: r.get(3)?,
        series: r.get(4)?,
        from: r.get(5)?,
        to: r.get(6)?,
        size: r.get(7)?,
        by: r.get(8)?,
        at: r.get(9)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebasing() {
        assert_eq!(
            rebase("manga/Starfall", "manga/Starfall", "x/Y").as_deref(),
            Some("x/Y")
        );
        assert_eq!(
            rebase("manga/Starfall/v01.cbz", "manga/Starfall", "x/Y").as_deref(),
            Some("x/Y/v01.cbz")
        );
        assert_eq!(
            rebase("manga/Starfall Zero/v01.cbz", "manga/Starfall", "x"),
            None
        );
    }
}
