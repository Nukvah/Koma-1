//! Organising the library: the changes curators make (moving, merging,
//! renaming, the trash, details), planned, carried out by the worker, and
//! undone from the journal.
//!
//! `koma serve` checks who may make a change, shows its plan (a dry run:
//! [`plan`]) and queues it in accounts.db. The worker takes each change in
//! order ([`run`]), carries it out a step at a time, appending one journal
//! line per file it touched ([`crate::journal`]), and records its progress
//! in the catalogue; between steps it looks whether the change was paused
//! or cancelled. A change cut short by a stop or a crash is taken up again
//! where it was: every step checks whether it is already done.
//!
//! Every file stays inside `/library`: a delete moves it into
//! `/library/.koma-trash/<change>/…` (hidden, so scans never look there),
//! where it stays until the trash's keep time, so a delete is a rename,
//! instant on the same drive. Undo replays a change's journal backwards,
//! and only if every file is still as the change left it.

use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Value, json};

use crate::accounts::{Accounts, Op};
use crate::catalog::Catalog;
use crate::journal::{Entry, TrashItem, rebase};
use crate::log;

/// The trash folder, in the library.
pub const TRASH: &str = ".koma-trash";

/// Kinds of change.
pub const KINDS: [&str; 12] = [
    "move", "series", "merge", "trash", "restore", "erase", "empty", "details", "write", "undo",
    "rename", "tidy",
];

/// Changes that can be undone.
pub fn undoable(kind: &str) -> bool {
    matches!(
        kind,
        "move" | "series" | "merge" | "trash" | "restore" | "details" | "write" | "rename" | "tidy"
    )
}

/// One step of a plan: a file or folder moved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub book: Option<i64>,
    /// A whole series folder (`folder`), or the book's series.
    pub series: Option<i64>,
    pub from: String,
    pub to: String,
    /// A photo folder (a book that is a folder).
    pub photos: bool,
    /// The step moves a series folder.
    pub folder: bool,
    /// The name was taken: `to` has a “(2)” added.
    pub clash: bool,
    /// Left out, as asked.
    pub skip: bool,
}

/// What a change will do, before it does it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub steps: Vec<Step>,
    /// A series folder left empty, removed at the end (a merge).
    pub remove_after: Option<String>,
}

/// A name for a folder or file Koma-1 makes: not empty, no `/`, not hidden,
/// no control characters, at most 200 bytes.
pub fn safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with('.')
        && !s.contains('/')
        && !s.contains('\\')
        && !s.chars().any(char::is_control)
        && s.trim() == s
}

/// A path relative to the library, made of safe names (one may be the
/// owner's `~name`).
pub fn safe_path(s: &str) -> bool {
    s.split('/')
        .all(|p| safe_name(p) || (p.starts_with('~') && safe_name(&p[1..])))
        && !s.is_empty()
}

fn join(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/")
}

/// `dir/Name v09.cbz` → `dir/Name v09 (2).cbz`; a folder `dir/Name` →
/// `dir/Name (2)`.
pub fn numbered(path: &str, n: u32, folder: bool) -> String {
    let (dir, file) = path.rsplit_once('/').map_or(("", path), |(d, f)| (d, f));
    let file = match file.rsplit_once('.') {
        Some((stem, ext)) if !folder && !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{file} ({n})"),
    };
    join(&[dir, &file])
}

fn exists(library: &Path, rel: &str) -> bool {
    fs::symlink_metadata(library.join(rel)).is_ok()
}

/// `to`, or a numbered one if it is taken on disk or by an earlier step.
fn free(library: &Path, to: &str, folder: bool, taken: &[String]) -> (String, bool) {
    let busy = |p: &str| exists(library, p) || taken.iter().any(|t| t == p);
    if !busy(to) {
        return (to.to_owned(), false);
    }
    (2..1000)
        .map(|n| numbered(to, n, folder))
        .find(|p| !busy(p))
        .map_or((to.to_owned(), true), |p| (p, true))
}

/// Where a move goes: an existing series' folder, or a new one.
fn target(c: &Catalog, to: &Value) -> Result<String, String> {
    if let Some(id) = to.get("series").and_then(Value::as_i64) {
        let s = c
            .series_by_id(id)
            .map_err(|e| e.to_string())?
            .ok_or("that series is gone")?;
        if s.path.starts_with('\0') || s.path.is_empty() {
            return Err(format!(
                "{} is loose books in the library's root, not a folder: move them into a series first",
                s.name
            ));
        }
        return Ok(s.path);
    }
    let text = |k: &str| {
        to.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let (library, collection, name) = (text("library"), text("collection"), text("name"));
    if !safe_name(&name) {
        return Err("a series name, please: no “/”, not starting with “.”".into());
    }
    for part in [&library, &collection] {
        if !part.is_empty() && !safe_path(part) {
            return Err(format!("not a folder name: {part}"));
        }
    }
    Ok(join(&[&library, &collection, &name]))
}

fn ids(v: &Value, key: &str) -> Vec<i64> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

/// The plan of a `move`, `series` or `merge` change: every file or folder
/// it moves, with clashes numbered. Read-only: the catalogue and the disk
/// are only looked at.
pub fn plan(c: &Catalog, library: &Path, kind: &str, p: &Value) -> Result<Plan, String> {
    let err = |e: crate::catalog::Error| e.to_string();
    let skip = ids(p, "skip");
    let mut out = Plan::default();
    let mut taken: Vec<String> = Vec::new();
    let add_book =
        |out: &mut Plan, id: i64, folder: &str, taken: &mut Vec<String>| -> Result<(), String> {
            let book = c.book(id).map_err(err)?.ok_or("a book is gone")?;
            let file = book.path.rsplit('/').next().unwrap_or(&book.path);
            let to = join(&[folder, file]);
            if to == book.path {
                return Ok(());
            }
            let (to, clash) = free(library, &to, book.photos, taken);
            taken.push(to.clone());
            out.steps.push(Step {
                book: Some(id),
                series: Some(book.series_id),
                from: book.path,
                to,
                photos: book.photos,
                folder: false,
                clash,
                skip: skip.contains(&id),
            });
            Ok(())
        };
    match kind {
        // New names in the same folders (the naming standard).
        "rename" => {
            let list = p
                .get("books")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for item in list {
                let id = item.get("id").and_then(Value::as_i64).unwrap_or(0);
                let name = item.get("to").and_then(Value::as_str).unwrap_or_default();
                if !safe_name(name) {
                    return Err(format!("not a file name: {name}"));
                }
                let book = c.book(id).map_err(err)?.ok_or("a book is gone")?;
                let folder = crate::scan::folder_of_path(&book.path);
                let to = join(&[folder, name]);
                if to == book.path {
                    continue;
                }
                let (to, clash) = free(library, &to, book.photos, &taken);
                taken.push(to.clone());
                out.steps.push(Step {
                    book: Some(id),
                    series: Some(book.series_id),
                    from: book.path,
                    to,
                    photos: book.photos,
                    folder: false,
                    clash,
                    skip: false,
                });
            }
        }
        "move" => {
            let folder = target(c, p.get("to").unwrap_or(&Value::Null))?;
            for id in ids(p, "books") {
                add_book(&mut out, id, &folder, &mut taken)?;
            }
        }
        "merge" => {
            let from = c
                .series_by_id(p.get("from").and_then(Value::as_i64).unwrap_or(0))
                .map_err(err)?
                .ok_or("that series is gone")?;
            let into = p.get("into").and_then(Value::as_i64).unwrap_or(0);
            if into == from.id {
                return Err("a series can't be merged into itself".into());
            }
            let folder = target(c, &json!({ "series": into }))?;
            for b in c.books(from.id).map_err(err)? {
                add_book(&mut out, b.id, &folder, &mut taken)?;
            }
            if !from.path.starts_with('\0') {
                out.remove_after = Some(from.path);
            }
        }
        "series" => {
            let s = c
                .series_by_id(p.get("series").and_then(Value::as_i64).unwrap_or(0))
                .map_err(err)?
                .ok_or("that series is gone")?;
            // Loose books, or books straight in a library folder (which
            // holds other series too): not a folder of its own.
            let holds_others = c
                .series()
                .map_err(err)?
                .iter()
                .any(|o| o.id != s.id && rebase(&o.path, &s.path, "x").is_some());
            if s.path.starts_with('\0') || holds_others {
                return Err(format!(
                    "{} is not a folder of its own: move its books instead",
                    s.name
                ));
            }
            let to = target(
                c,
                &json!({
                    "library": p.get("library").cloned().unwrap_or_default(),
                    "collection": p.get("collection").cloned().unwrap_or_default(),
                    "name": p.get("name").cloned().unwrap_or_default(),
                }),
            )?;
            if to == s.path {
                return Ok(out);
            }
            if rebase(&to, &s.path, "").is_some() {
                return Err("a folder can't move into itself".into());
            }
            let (to, clash) = free(library, &to, true, &taken);
            out.steps.push(Step {
                book: None,
                series: Some(s.id),
                from: s.path,
                to,
                photos: false,
                folder: true,
                clash,
                skip: false,
            });
        }
        _ => return Err(format!("no plan for {kind}")),
    }
    Ok(out)
}

/// A change the worker is carrying out.
struct Run<'a> {
    library: &'a Path,
    catalog: &'a mut Catalog,
    /// To look for a pause or a cancel; none for the worker's own changes.
    accounts: Option<&'a Accounts>,
    op: &'a Op,
    done: i64,
    total: i64,
}

/// Why a change stopped early.
enum Stop {
    Paused,
    Cancelled,
    Failed(String),
}

impl From<crate::catalog::Error> for Stop {
    fn from(e: crate::catalog::Error) -> Self {
        Stop::Failed(format!("catalogue: {e}"))
    }
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Stop::Failed(e.to_string())
    }
}

impl Run<'_> {
    fn abs(&self, rel: &str) -> std::path::PathBuf {
        self.library.join(rel)
    }

    /// Between steps: progress, and whether to go on.
    fn step(&mut self, message: &str) -> Result<(), Stop> {
        let control = self
            .accounts
            .and_then(|a| a.op(self.op.id).ok().flatten())
            .map(|o| o.control)
            .unwrap_or_default();
        match control.as_str() {
            "cancel" => return Err(Stop::Cancelled),
            "pause" => return Err(Stop::Paused),
            _ => {}
        }
        self.catalog
            .set_op(self.op.id, "running", self.done, self.total, message)?;
        Ok(())
    }

    fn journal(
        &self,
        action: &str,
        book: Option<i64>,
        series: Option<i64>,
        from: &str,
        to: &str,
        data: Option<String>,
    ) -> Result<(), Stop> {
        let (size, mtime) = match fs::symlink_metadata(self.abs(to)) {
            Ok(m) if m.is_file() => (Some(m.len() as i64), Some(crate::scan::mtime_ns(&m))),
            _ => (None, None),
        };
        self.catalog.journal_add(&Entry {
            seq: 0,
            op: self.op.id,
            action: action.into(),
            book,
            series,
            from: from.into(),
            to: to.into(),
            size,
            mtime,
            data,
            at: 0,
        })?;
        Ok(())
    }

    /// Makes `rel`'s missing folders, journaling each, so undo can remove
    /// them again.
    fn make_dirs(&self, rel: &str) -> Result<(), Stop> {
        let mut path = String::new();
        for part in rel.split('/').filter(|p| !p.is_empty()) {
            path = join(&[&path, part]);
            if !exists(self.library, &path) {
                fs::create_dir(self.abs(&path))?;
                self.journal("mkdir", None, None, "", &path, None)?;
            }
        }
        Ok(())
    }

    /// Renames `from` to `to` (both relative), making `to`'s folder.
    fn rename(&self, from: &str, to: &str) -> Result<(), Stop> {
        if let Some((dir, _)) = to.rsplit_once('/') {
            self.make_dirs(dir)?;
        }
        if exists(self.library, to) {
            return Err(Stop::Failed(format!("{to} is taken")));
        }
        fs::rename(self.abs(from), self.abs(to)).map_err(|e| {
            if e.raw_os_error() == Some(18) {
                Stop::Failed(format!(
                    "{from} and {to} are on different drives: moving between them is not supported"
                ))
            } else {
                Stop::Failed(format!("{from} → {to}: {e}"))
            }
        })
    }

    fn moves(&mut self, plan: &Plan) -> Result<(), Stop> {
        self.total = plan.steps.len() as i64;
        for s in &plan.steps {
            self.step(&format!("{} → {}", s.from, s.to))?;
            if !s.skip {
                let there = exists(self.library, &s.to);
                if exists(self.library, &s.from) {
                    self.rename(&s.from, &s.to)?;
                } else if !there {
                    return Err(Stop::Failed(format!("{} is gone", s.from)));
                }
                // (Already there: a change taken up again after a stop.)
                if s.folder {
                    self.catalog.move_folder(&s.from, &s.to)?;
                    self.journal("folder", None, s.series, &s.from, &s.to, None)?;
                } else if let Some(b) = s.book {
                    self.catalog.move_book(b, &s.to, s.photos)?;
                    self.journal("move", Some(b), s.series, &s.from, &s.to, None)?;
                }
            }
            self.done += 1;
        }
        if let Some(dir) = &plan.remove_after {
            // Only if nothing is left in it.
            if fs::remove_dir(self.abs(dir)).is_ok() {
                self.journal("rmdir", None, None, dir, "", None)?;
            }
        }
        Ok(())
    }

    fn trash(&mut self, p: &Value) -> Result<(), Stop> {
        let series = ids(p, "series");
        let books = ids(p, "books");
        self.total = (series.len() + books.len()) as i64;
        let now = crate::accounts::now();
        for id in series {
            self.step("to the trash")?;
            let Some(s) = self.catalog.series_by_id(id)? else {
                self.done += 1;
                continue;
            };
            if s.path.starts_with('\0') {
                // Loose books: each one on its own.
                let paths = self.catalog.book_paths(id)?;
                for (b, ..) in paths {
                    self.trash_book(b, now)?;
                }
                self.done += 1;
                continue;
            }
            let present = self.catalog.book_paths(id)?;
            let size: i64 = present
                .iter()
                .filter_map(|(b, ..)| self.catalog.book(*b).ok().flatten())
                .map(|b| b.size as i64)
                .sum();
            let to = format!("{TRASH}/{}/{}", self.op.id, s.path);
            if exists(self.library, &s.path) {
                self.rename(&s.path, &to)?;
            }
            let item = TrashItem {
                id: 0,
                op: self.op.id,
                kind: "series".into(),
                name: s.name.clone(),
                series: s.id,
                from: s.path.clone(),
                to: to.clone(),
                size,
                by: self.op.by.clone(),
                at: now,
            };
            let ids: Vec<i64> = present.iter().map(|(b, ..)| *b).collect();
            let item_id = self.catalog.trash_add(&item, &ids)?;
            self.journal(
                "trash",
                None,
                Some(s.id),
                &s.path,
                &to,
                Some(json!({ "item": item_id }).to_string()),
            )?;
            self.done += 1;
        }
        for id in books {
            self.step("to the trash")?;
            self.trash_book(id, now)?;
            self.done += 1;
        }
        Ok(())
    }

    fn trash_book(&mut self, id: i64, now: i64) -> Result<(), Stop> {
        let Some(book) = self.catalog.book(id)? else {
            return Ok(());
        };
        let to = format!("{TRASH}/{}/{}", self.op.id, book.path);
        if exists(self.library, &book.path) {
            self.rename(&book.path, &to)?;
        }
        let item = TrashItem {
            id: 0,
            op: self.op.id,
            kind: "book".into(),
            name: book.name.clone(),
            series: book.series_id,
            from: book.path.clone(),
            to: to.clone(),
            size: book.size as i64,
            by: self.op.by.clone(),
            at: now,
        };
        let item_id = self.catalog.trash_add(&item, &[id])?;
        self.journal(
            "trash",
            Some(id),
            Some(book.series_id),
            &book.path,
            &to,
            Some(json!({ "item": item_id }).to_string()),
        )
    }

    fn restore(&mut self, items: &[i64]) -> Result<(), Stop> {
        self.total = items.len() as i64;
        for &id in items {
            self.step("restoring")?;
            if let Some(item) = self.catalog.trash_item(id)? {
                self.restore_item(&item, None)?;
            }
            self.done += 1;
        }
        Ok(())
    }

    /// Puts a trash item back where it was (or at `exact`), numbered if the
    /// name is taken now.
    fn restore_item(&mut self, item: &TrashItem, exact: Option<&str>) -> Result<String, Stop> {
        let books = self.catalog.trash_books(item.id)?;
        let target = match exact {
            Some(t) => t.to_owned(),
            None => {
                free(
                    self.library,
                    &item.from,
                    item.kind == "series" || books.first().is_some_and(|b| b.2),
                    &[],
                )
                .0
            }
        };
        if exists(self.library, &item.to) {
            self.rename(&item.to, &target)?;
        } else if !exists(self.library, &target) {
            return Err(Stop::Failed(format!(
                "{} is no longer in the trash",
                item.name
            )));
        }
        self.catalog.trash_restored(item, &target)?;
        let data = json!({
            "item": { "kind": item.kind, "name": item.name, "series": item.series,
                      "from": item.from, "to": item.to, "size": item.size, "by": item.by, "at": item.at },
            "books": books.iter().map(|b| b.0).collect::<Vec<_>>(),
        });
        self.journal(
            "restore",
            None,
            Some(item.series),
            &item.to,
            &target,
            Some(data.to_string()),
        )?;
        tidy_trash(self.library, &item.to);
        Ok(target)
    }

    fn erase(&mut self, items: &[i64]) -> Result<(), Stop> {
        self.total = items.len() as i64;
        for &id in items {
            self.step("erasing")?;
            if let Some(item) = self.catalog.trash_item(id)? {
                erase_item(self.library, self.catalog, &item)?;
                self.journal("erase", None, Some(item.series), &item.to, "", None)?;
            }
            self.done += 1;
        }
        Ok(())
    }

    fn details(&mut self, p: &Value) -> Result<(), Stop> {
        let books = ids(p, "books");
        let set = p
            .get("set")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        self.total = books.len() as i64;
        for &b in &books {
            self.step("details")?;
            let mut edits: Vec<(String, String)> = self.catalog.edits(b)?;
            for (field, value) in &set {
                edits.retain(|(f, _)| f != field);
                if !value.is_null() {
                    edits.push((field.clone(), value.to_string()));
                }
            }
            let before = self.catalog.set_edits(b, &edits)?;
            let data = Value::Object(
                before
                    .into_iter()
                    .map(|(f, v)| (f, serde_json::from_str(&v).unwrap_or(Value::Null)))
                    .collect(),
            );
            self.journal("edit", Some(b), None, "", "", Some(data.to_string()))?;
            self.done += 1;
        }
        if p.get("write").and_then(Value::as_bool) == Some(true) {
            self.done = 0;
            self.write(&books)?;
        }
        // Search shows the new names and tags.
        self.catalog.rebuild_search()?;
        Ok(())
    }

    /// Writes each book's details as `metadata.json` next to it; one that
    /// was there is kept in the trash folder for undo.
    fn write(&mut self, books: &[i64]) -> Result<(), Stop> {
        self.total = books.len() as i64;
        for &b in books {
            self.step("writing metadata.json")?;
            let Some(book) = self.catalog.book(b)? else {
                continue;
            };
            let side = crate::scan::sidecar_of(&book.path, book.photos);
            if exists(self.library, &side) {
                let backup = format!("{TRASH}/{}/{side}", self.op.id);
                self.rename(&side, &backup)?;
                self.journal("backup", Some(b), None, &side, &backup, None)?;
            }
            let json = metadata_json(self.catalog, &book)?;
            let tmp = format!("{side}.koma-tmp");
            fs::write(self.abs(&tmp), json)?;
            fs::rename(self.abs(&tmp), self.abs(&side))?;
            self.journal("write", Some(b), None, "", &side, None)?;
            self.done += 1;
        }
        Ok(())
    }

    /// Undoes change `of`: its journal backwards, once every file is checked.
    fn undo(&mut self, of: i64) -> Result<(), Stop> {
        let entries = self.catalog.journal_of(of)?;
        if entries.is_empty() {
            return Err(Stop::Failed("that change did nothing to undo".into()));
        }
        for e in &entries {
            if let Err(why) = can_undo(self.library, self.catalog, e) {
                return Err(Stop::Failed(format!("undo not possible: {why}")));
            }
        }
        self.total = entries.len() as i64;
        // A tidy undone is not done again: its books keep the names they
        // get back.
        let tidy = of < 0
            && self
                .catalog
                .worker_op_kind(of)?
                .is_some_and(|(kind, _)| kind == "tidy");
        for e in entries.iter().rev() {
            self.step("undoing")?;
            self.undo_entry(e)?;
            if tidy && e.action == "move" {
                self.catalog.keep_name(&e.from)?;
            }
            self.done += 1;
        }
        self.catalog.rebuild_search()?;
        Ok(())
    }

    fn undo_entry(&mut self, e: &Entry) -> Result<(), Stop> {
        let data: Value = e
            .data
            .as_deref()
            .and_then(|d| serde_json::from_str(d).ok())
            .unwrap_or_default();
        match e.action.as_str() {
            "move" => {
                let photos = fs::symlink_metadata(self.abs(&e.to)).is_ok_and(|m| m.is_dir());
                self.rename(&e.to, &e.from)?;
                if let Some(b) = e.book {
                    self.catalog.move_book(b, &e.from, photos)?;
                }
                self.journal("move", e.book, e.series, &e.to, &e.from, None)?;
            }
            "folder" => {
                self.rename(&e.to, &e.from)?;
                self.catalog.move_folder(&e.to, &e.from)?;
                self.journal("folder", None, e.series, &e.to, &e.from, None)?;
            }
            "mkdir" => {
                let _ = fs::remove_dir(self.abs(&e.to));
            }
            "rmdir" => {
                let _ = fs::create_dir_all(self.abs(&e.from));
            }
            "trash" => {
                if let Some(item) = data
                    .get("item")
                    .and_then(Value::as_i64)
                    .map(|id| self.catalog.trash_item(id))
                    .transpose()?
                    .flatten()
                {
                    self.restore_item(&item, Some(&e.from))?;
                }
            }
            "restore" => {
                // Back into the trash where it was.
                let item = data.get("item").cloned().unwrap_or_default();
                let s = |k: &str| {
                    item.get(k)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                let i = |k: &str| item.get(k).and_then(Value::as_i64).unwrap_or_default();
                let books = ids(&data, "books");
                self.rename(&e.to, &e.from)?;
                let restored = TrashItem {
                    id: 0,
                    op: self.op.id,
                    kind: s("kind"),
                    name: s("name"),
                    series: i("series"),
                    from: e.to.clone(),
                    to: e.from.clone(),
                    size: i("size"),
                    by: s("by"),
                    at: i("at"),
                };
                // The catalogue still has the books at the restored place.
                self.catalog.trash_add(&restored, &books)?;
                self.journal("trash", None, e.series, &e.to, &e.from, None)?;
            }
            "edit" => {
                if let (Some(b), Value::Object(before)) = (e.book, data) {
                    let edits: Vec<(String, String)> = before
                        .into_iter()
                        .map(|(f, v)| (f, v.to_string()))
                        .collect();
                    self.catalog.set_edits(b, &edits)?;
                }
            }
            "write" => {
                fs::remove_file(self.abs(&e.to))?;
            }
            "backup" => {
                self.rename(&e.to, &e.from)?;
            }
            _ => {}
        }
        Ok(())
    }
}

/// Whether a journal line can be undone: what it left is still there,
/// unchanged, and its old place is free.
fn can_undo(library: &Path, c: &Catalog, e: &Entry) -> Result<(), String> {
    let unchanged = |rel: &str| -> Result<(), String> {
        let m = fs::symlink_metadata(library.join(rel)).map_err(|_| format!("{rel} is gone"))?;
        if let (Some(size), Some(mtime)) = (e.size, e.mtime)
            && (m.len() as i64 != size || crate::scan::mtime_ns(&m) != mtime)
        {
            return Err(format!("{rel} changed since"));
        }
        Ok(())
    };
    match e.action.as_str() {
        "move" | "folder" => {
            unchanged(&e.to)?;
            if exists(library, &e.from) {
                return Err(format!("{} is taken now", e.from));
            }
        }
        "write" | "backup" => unchanged(&e.to)?,
        "trash" => {
            let item = e
                .data
                .as_deref()
                .and_then(|d| serde_json::from_str::<Value>(d).ok())
                .and_then(|d| d.get("item").and_then(Value::as_i64));
            match item.map(|i| c.trash_item(i)) {
                Some(Ok(Some(_))) => {}
                _ => return Err("it was restored or erased since".into()),
            }
            if exists(library, &e.from) {
                return Err(format!("{} is taken now", e.from));
            }
        }
        "restore" => unchanged(&e.to)?,
        "erase" => return Err("it was erased for good".into()),
        _ => {}
    }
    Ok(())
}

/// Removes a trash item's files and forgets its books.
fn erase_item(library: &Path, c: &mut Catalog, item: &TrashItem) -> Result<(), Stop> {
    let path = library.join(&item.to);
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(&path)?,
        Ok(_) => fs::remove_file(&path)?,
        Err(_) => {}
    }
    c.trash_erased(item.id)?;
    tidy_trash(library, &item.to);
    Ok(())
}

/// Removes the trash's folders left empty above `rel`.
fn tidy_trash(library: &Path, rel: &str) {
    let mut dir = rel;
    while let Some((parent, _)) = dir.rsplit_once('/') {
        if parent == TRASH || fs::remove_dir(library.join(parent)).is_err() {
            break;
        }
        dir = parent;
    }
}

/// A book's details as the `metadata.json` Koma-1 reads (see
/// [`crate::meta::metadata_json`]).
fn metadata_json(c: &Catalog, book: &crate::catalog::Book) -> Result<String, Stop> {
    let d = c.details(&book.path)?.unwrap_or_default();
    let series = c
        .series_by_id(book.series_id)?
        .map(|s| s.name)
        .unwrap_or_default();
    let mut o = serde_json::Map::new();
    let mut put = |k: &str, v: Value| {
        let empty =
            v.is_null() || v.as_str() == Some("") || v.as_array().is_some_and(Vec::is_empty);
        if !empty {
            o.insert(k.into(), v);
        }
    };
    put("title", json!(d.title));
    put("series", json!(series));
    if let Some(n) = d.number {
        put("number", json!(n));
    }
    put("summary", json!(d.summary));
    put("artists", json!(d.authors));
    put("groups", json!(d.publishers));
    put("genres", json!(d.genres));
    put("tags", json!(d.tags));
    put("year", d.year.map_or(Value::Null, |y| json!(y)));
    put("language", json!(d.language));
    put("direction", d.direction.map_or(Value::Null, |x| json!(x)));
    put("thumbnail", d.cover_page.map_or(Value::Null, |x| json!(x)));
    serde_json::to_string_pretty(&Value::Object(o)).map_err(|e| Stop::Failed(e.to_string()))
}

/// Carries out one change, recording how it went.
fn run_op(library: &Path, catalog: &mut Catalog, accounts: &Accounts, op: &Op) {
    if op.control == "pause" {
        if catalog.op_state(op.id).ok().flatten().is_none() {
            let _ = catalog.set_op(op.id, "paused", 0, 0, "paused before it started");
        }
        return;
    }
    let p: Value = serde_json::from_str(&op.payload).unwrap_or_default();
    let mut run = Run {
        library,
        catalog,
        accounts: Some(accounts),
        op,
        done: 0,
        total: 0,
    };
    let result = (|| -> Result<(), Stop> {
        run.step("starting")?;
        match op.kind.as_str() {
            "move" | "series" | "merge" | "rename" => {
                let plan = plan(run.catalog, library, &op.kind, &p).map_err(Stop::Failed)?;
                run.moves(&plan)
            }
            "trash" => run.trash(&p),
            "restore" => run.restore(&ids(&p, "items")),
            "erase" => run.erase(&ids(&p, "items")),
            "empty" => {
                let all: Vec<i64> = run.catalog.trash_items()?.iter().map(|t| t.id).collect();
                run.erase(&all)
            }
            "details" => run.details(&p),
            "write" => run.write(&ids(&p, "books")),
            "undo" => run.undo(p.get("op").and_then(Value::as_i64).unwrap_or(0)),
            other => Err(Stop::Failed(format!("unknown change: {other}"))),
        }
    })();
    let (done, total) = (run.done, run.total);
    let (state, message) = match result {
        Ok(()) => ("done", String::new()),
        Err(Stop::Paused) => ("paused", "paused".to_owned()),
        Err(Stop::Cancelled) => (
            "cancelled",
            "cancelled: what was done stays, and can be undone".to_owned(),
        ),
        Err(Stop::Failed(why)) => ("failed", why),
    };
    if state == "failed" {
        log::error(
            "worker: a change failed",
            &[("change", &op.title), ("error", &message)],
        );
    } else if state == "done" {
        log::info(
            "worker: change done",
            &[("change", &op.title), ("by", &op.by)],
        );
    }
    if let Err(e) = catalog.set_op(op.id, state, done, total, &message) {
        log::error(
            "worker: cannot record a change",
            &[("error", &e.to_string())],
        );
    }
}

/// Takes up the changes queued in accounts.db that the catalogue has not
/// finished: paused ones that may go on, those cut short by a stop, then
/// new ones, oldest first.
pub fn run(library: &Path, data: &Path, catalog: &mut Catalog) {
    let Ok(Some(accounts)) = Accounts::open_read_only(&data.join("accounts.db")) else {
        return;
    };
    let mut todo: Vec<i64> = catalog.unfinished_ops().unwrap_or_default();
    if let Ok(last) = catalog.last_op()
        && let Ok(new) = accounts.ops_after(last)
    {
        todo.extend(new.iter().map(|o| o.id));
    }
    let ran = !todo.is_empty();
    for id in todo {
        let Ok(Some(op)) = accounts.op(id) else {
            continue;
        };
        if op.control == "cancel"
            && catalog
                .op_state(id)
                .ok()
                .flatten()
                .is_some_and(|s| s.state == "paused")
        {
            let _ = catalog.set_op(id, "cancelled", 0, 0, "cancelled");
            continue;
        }
        run_op(library, catalog, &accounts, &op);
    }
    // A set renamed in the fix-up queue (or moved) is placed at once.
    if ran {
        tidy(library, data, catalog);
    }
}

/// Keeps every cosplay library in `Cosplayer / Set / variant` folders: the
/// worker's own change (a negative id), journaled and undoable like any
/// other, run after each scan. Sets it cannot place are left for the
/// fix-up queue; a tidy undone is not done again.
pub fn tidy(library: &Path, data: &Path, catalog: &mut Catalog) {
    let Ok(folders) = crate::naming::cosplay_libraries(catalog) else {
        return;
    };
    let mut kept: std::collections::HashSet<String> = catalog
        .kept_names()
        .unwrap_or_default()
        .into_iter()
        .collect();
    if let Ok(Some(a)) = Accounts::open_read_only(&data.join("accounts.db")) {
        kept.extend(a.kept_names().unwrap_or_default());
    }
    for folder in folders {
        let Ok(rows) = crate::naming::tidy_rows(catalog, library, &folder, &kept) else {
            continue;
        };
        let steps: Vec<Step> = rows
            .iter()
            .filter(|r| r.status == crate::naming::Status::Rename)
            .map(|r| Step {
                book: Some(r.book),
                series: Some(r.series),
                from: r.from.clone(),
                to: r.to.clone(),
                photos: r.photos,
                folder: false,
                clash: false,
                skip: false,
            })
            .collect();
        if steps.is_empty() {
            continue;
        }
        let title = format!(
            "Tidy {folder}: {} into Cosplayer / Set folders",
            crate::html::plural(steps.len() as i64, "set", "sets")
        );
        let Ok(id) = catalog.new_worker_op("tidy", &title) else {
            continue;
        };
        let op = Op {
            id,
            account: None,
            by: "worker".into(),
            kind: "tidy".into(),
            payload: String::new(),
            title: title.clone(),
            control: String::new(),
            created: crate::accounts::now(),
        };
        let mut run = Run {
            library,
            catalog: &mut *catalog,
            accounts: None,
            op: &op,
            done: 0,
            total: 0,
        };
        let result = run.moves(&Plan {
            steps,
            remove_after: None,
        });
        let (done, total) = (run.done, run.total);
        let (state, message) = match result {
            Ok(()) => ("done", String::new()),
            Err(Stop::Failed(why)) => ("failed", why),
            Err(_) => ("cancelled", String::new()),
        };
        if state == "failed" {
            log::error(
                "worker: the cosplay tidy failed",
                &[("library", &folder), ("error", &message)],
            );
        } else {
            log::info(
                "worker: cosplay tidy",
                &[("library", &folder), ("sets", &done.to_string())],
            );
        }
        let _ = catalog.set_op(id, state, done, total, &message);
        let _ = catalog.rebuild_search();
    }
}

/// Erases what the trash has kept past its keep time.
pub fn expire_trash(library: &Path, data: &Path, catalog: &mut Catalog) {
    let days = Accounts::open_read_only(&data.join("accounts.db"))
        .ok()
        .flatten()
        .and_then(|a| a.trash_days().ok())
        .unwrap_or(30);
    if days <= 0 {
        return;
    }
    let before = crate::accounts::now() - days * 86_400;
    let Ok(items) = catalog.trash_items() else {
        return;
    };
    for item in items.into_iter().filter(|i| i.at < before) {
        match erase_item(library, catalog, &item) {
            Ok(()) => log::info("worker: erased from the trash", &[("name", &item.name)]),
            Err(Stop::Failed(e)) => log::error(
                "worker: cannot erase from the trash",
                &[("name", &item.name), ("error", &e)],
            ),
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_numbers() {
        assert!(safe_name("Starfall Zero"));
        for bad in ["", ".koma-trash", "a/b", "..", " x", "x\u{7}"] {
            assert!(!safe_name(bad), "{bad:?}");
        }
        assert!(safe_path("~mirelle/sketchbooks"));
        assert!(safe_path("manga/Starfall Saga"));
        assert!(!safe_path("manga/../etc"));
        assert_eq!(
            numbered("m/Starfall v09.cbz", 2, false),
            "m/Starfall v09 (2).cbz"
        );
        assert_eq!(numbered("m/Starfall", 3, true), "m/Starfall (3)");
        assert_eq!(numbered("Set 01", 2, false), "Set 01 (2)");
    }
}
