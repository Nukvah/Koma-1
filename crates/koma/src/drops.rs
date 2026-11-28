//! What a drop of files and folders becomes (docs/design/README.md,
//! "Uploads"). The browser sends the dropped paths and sizes, and the text of
//! any metadata file among them; the server answers with the plan, so the
//! rules live here only. Nothing here touches the disk: the plan is a pure
//! function of the drop, which `tests/drops.rs` pins with a golden set.
//!
//! Manga and comics:
//!
//! | You drop… | It becomes… |
//! |---|---|
//! | a folder of images | a series with one chapter, packed into `<folder>.cbz` |
//! | a folder of image folders | a series, one chapter per folder |
//! | a folder of numbered archives of one series | that series, named after the folder |
//! | a folder of unrelated archives, or of series folders | a collection |
//! | image folders side by side, `Name 1`, `Name 2` | one series `Name`, a chapter each |
//! | loose archives | each in the series its name says |
//!
//! Cosplay: every set in a drop (a folder of photos, or an archive) goes to
//! `Cosplayer / Set /`, the cosplayer from its name or else from the folder
//! it was dropped in; photos are packed into `<name>.zip`, and a folder of
//! archives is one set with each archive a variant.
//!
//! Dropped onto a series (or a cosplay set) everything goes into it; onto a
//! collection (or a cosplayer), into that.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::archive;
use crate::meta::{self, Meta};
use crate::names;
use crate::naming;
use crate::natsort;

/// Most files one drop may hold.
pub const MAX_ITEMS: usize = 50_000;
/// Longest name a planned file or folder gets, in characters.
const MAX_NAME: usize = 150;

/// One dropped file: its path inside the drop (`/`-separated) and size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub path: String,
    pub size: u64,
}

/// Where a drop was let go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A library page: the drop makes its own series and collections.
    Library,
    /// A collection (or, in a cosplay library, a cosplayer's folder).
    Collection(String),
    /// A series (or a cosplay set): everything goes into it.
    Series(i64),
}

/// A drop: what was dropped, onto what.
#[derive(Clone, Debug)]
pub struct Drop {
    pub items: Vec<Item>,
    /// Parsed metadata files, by item index.
    pub meta: BTreeMap<usize, Meta>,
    pub target: Target,
    pub cosplay: bool,
    /// Today's date, `2026-11-28`, for loose images that have no name.
    pub today: String,
}

/// Where a planned file goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// Into an existing series (or set), by id.
    Series(i64),
    /// `<library>/[<collection>/][<folder>/]`, folders made as needed.
    Folder {
        collection: Option<String>,
        folder: Option<String>,
    },
}

/// What a planned file is made of.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum What {
    /// One dropped file, sent as it is (CB7 is converted on arrival).
    File(usize),
    /// Images packed into one archive on the server, in this order, with
    /// metadata files added after them.
    Pack {
        pages: Vec<usize>,
        extras: Vec<usize>,
    },
}

/// One file the drop makes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub what: What,
    /// The file's name as dropped (or the folder's, for a pack), with the
    /// extension it gets.
    pub name: String,
    pub place: Place,
    pub size: u64,
    /// In a cosplay library: the set's place, as read from the drop.
    pub set: Option<SetPlace>,
    /// The name it lands with under the naming standard ([`name_to_standard`]).
    pub to: Option<String>,
    /// Why it keeps its dropped name instead.
    pub note: Option<String>,
}

/// The plan for a drop.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    pub jobs: Vec<Job>,
    /// Files that cannot be taken, with the reason.
    pub refused: Vec<(usize, String)>,
    /// Files left out without a word: system files, text files.
    pub ignored: Vec<usize>,
}

/// What kind of file a dropped item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sort {
    Image,
    Video,
    Book,
    Meta,
    /// Read from M11 on.
    Rar,
    Other,
}

const VIDEO_EXTENSIONS: [&str; 5] = ["mp4", "webm", "mkv", "mov", "m4v"];
const BOOK_EXTENSIONS: [&str; 5] = ["cbz", "zip", "cb7", "7z", "epub"];

fn extension(name: &str) -> Option<&str> {
    name.rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, ext)| ext)
}

fn has_extension(name: &str, list: &[&str]) -> bool {
    extension(name).is_some_and(|e| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

fn sort_of(name: &str) -> Sort {
    if meta::layer_of(name).is_some() {
        Sort::Meta
    } else if archive::is_page_name(name) {
        Sort::Image
    } else if has_extension(name, &BOOK_EXTENSIONS) {
        Sort::Book
    } else if has_extension(name, &VIDEO_EXTENSIONS) {
        Sort::Video
    } else if has_extension(name, &["cbr", "rar"]) {
        Sort::Rar
    } else {
        Sort::Other
    }
}

/// Whether a file can go into a pack: an image, a video or a metadata file.
pub fn packs_member(name: &str) -> bool {
    matches!(sort_of(name), Sort::Image | Sort::Video | Sort::Meta)
}

/// Files nobody means to upload: hidden ones, macOS and Windows leftovers.
fn is_junk(path: &str) -> bool {
    archive::is_hidden(path)
        || path.rsplit('/').next().is_some_and(|n| {
            n.eq_ignore_ascii_case("thumbs.db") || n.eq_ignore_ascii_case("desktop.ini")
        })
}

/// The file name part of a path.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn stem_of(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => name,
    }
}

/// A name that can be a file or folder name: no separators or characters
/// Windows refuses, no leading dot, at most [`MAX_NAME`] characters.
pub fn safe_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let joined = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim_start_matches('.').trim();
    let cut: String = trimmed.chars().take(MAX_NAME).collect();
    let cut = cut.trim_end().trim_end_matches('.').to_owned();
    if cut.is_empty() {
        "Untitled".to_owned()
    } else {
        cut
    }
}

/// A folder of the drop.
#[derive(Default)]
struct Node {
    name: String,
    dirs: Vec<Node>,
    files: Vec<usize>,
}

impl Node {
    /// Puts every folder's folders in natural order, so a plan does not
    /// depend on the order the browser listed them in.
    fn sort(&mut self) {
        self.dirs.sort_by(|a, b| natsort::compare(&a.name, &b.name));
        for d in &mut self.dirs {
            d.sort();
        }
    }

    fn dir(&mut self, name: &str) -> &mut Node {
        match self.dirs.iter().position(|d| d.name == name) {
            Some(i) => &mut self.dirs[i],
            None => {
                self.dirs.push(Node {
                    name: name.to_owned(),
                    ..Node::default()
                });
                self.dirs.last_mut().expect("just pushed")
            }
        }
    }
}

/// The series name and number a name says (`Starfall Ch.1` → `Starfall`, 1).
fn series_and_number(name: &str) -> (String, Option<f64>) {
    let (_, number, before) = names::parse_number(name);
    (names::tidy(&before), number)
}

/// The series a loose archive belongs to, from its name.
fn series_of_file(name: &str) -> String {
    let n = names::book_name(stem_of(name));
    n.series.unwrap_or_else(|| names::tidy(stem_of(name)))
}

struct Planner<'a> {
    drop: &'a Drop,
    sorts: Vec<Sort>,
    plan: Plan,
}

impl<'a> Planner<'a> {
    fn name(&self, i: usize) -> &str {
        file_name(&self.drop.items[i].path)
    }

    fn size(&self, files: &[usize]) -> u64 {
        files.iter().map(|&i| self.drop.items[i].size).sum()
    }

    fn sorted(&self, mut files: Vec<usize>) -> Vec<usize> {
        files.sort_by(|&a, &b| natsort::compare(self.name(a), self.name(b)).then(a.cmp(&b)));
        files
    }

    fn of(&self, n: &Node, sort: Sort) -> Vec<usize> {
        self.sorted(
            n.files
                .iter()
                .copied()
                .filter(|&i| self.sorts[i] == sort)
                .collect(),
        )
    }

    fn images(&self, n: &Node) -> Vec<usize> {
        self.of(n, Sort::Image)
    }

    fn books(&self, n: &Node) -> Vec<usize> {
        self.of(n, Sort::Book)
    }

    /// A folder's media in order: its photos with any videos between them.
    fn media(&self, n: &Node) -> Vec<usize> {
        self.sorted(
            n.files
                .iter()
                .copied()
                .filter(|&i| matches!(self.sorts[i], Sort::Image | Sort::Video))
                .collect(),
        )
    }

    fn metas(&self, n: &Node) -> Vec<usize> {
        self.sorted(
            n.files
                .iter()
                .copied()
                .filter(|&i| self.sorts[i] == Sort::Meta)
                .collect(),
        )
    }

    /// A folder of images and nothing else to plan: a chapter.
    fn is_image_folder(&self, n: &Node) -> bool {
        n.dirs.is_empty() && !self.images(n).is_empty() && self.books(n).is_empty()
    }

    /// What the folder's metadata files say (metadata.json over ComicInfo.xml).
    fn folder_meta(&self, n: &Node) -> Meta {
        let mut layers: Vec<(usize, &Meta)> = n
            .files
            .iter()
            .filter_map(|i| Some((meta::layer_of(self.name(*i))?, self.drop.meta.get(i)?)))
            .collect();
        layers.sort_by_key(|(layer, _)| *layer);
        layers
            .into_iter()
            .fold(Meta::default(), |weak, (_, strong)| {
                meta::merge(weak, strong.clone())
            })
    }

    /// The title of the book a folder packs into.
    fn title_of(&self, n: &Node) -> String {
        let m = self.folder_meta(n);
        let title = if !m.title.is_empty() {
            m.title
        } else if !m.series.is_empty() && !m.number.is_empty() {
            format!("{} {}", m.series, m.number)
        } else {
            String::new()
        };
        let title = names::tidy(&title);
        if title.is_empty() {
            n.name.clone()
        } else {
            title
        }
    }

    /// The series a folder of images is in, by its metadata or its name.
    fn series_of(&self, n: &Node) -> String {
        let m = self.folder_meta(n);
        let s = names::tidy(if m.series.is_empty() {
            &m.title
        } else {
            &m.series
        });
        if s.is_empty() { n.name.clone() } else { s }
    }

    /// Archives of one series: numbered the same way (`Name 01`, `Name 02`,
    /// and a plain `Name` as part 1).
    fn related(&self, files: &[usize]) -> bool {
        if files.len() < 2 {
            return true;
        }
        let parsed: Vec<(String, Option<f64>)> = files
            .iter()
            .map(|&i| series_and_number(stem_of(self.name(i))))
            .collect();
        let mut keys: Vec<String> = parsed
            .iter()
            .filter(|(_, n)| n.is_some())
            .map(|(s, _)| s.to_lowercase())
            .collect();
        keys.sort();
        keys.dedup();
        let loose: Vec<String> = parsed
            .iter()
            .filter(|(_, n)| n.is_none())
            .map(|(s, _)| s.to_lowercase())
            .collect();
        keys.len() == 1 && (loose.is_empty() || (loose.len() == 1 && keys.contains(&loose[0])))
    }

    fn file(&mut self, i: usize, place: Place) {
        let name = self.name(i).to_owned();
        let size = self.drop.items[i].size;
        self.plan.jobs.push(Job {
            what: What::File(i),
            name,
            place,
            size,
            set: None,
            to: None,
            note: None,
        });
    }

    fn pack(&mut self, name: &str, pages: Vec<usize>, extras: Vec<usize>, place: Place) {
        let ext = if self.drop.cosplay { "zip" } else { "cbz" };
        let size = self.size(&pages) + self.size(&extras);
        self.plan.jobs.push(Job {
            what: What::Pack { pages, extras },
            name: format!("{}.{ext}", safe_name(name)),
            place,
            size,
            set: None,
            to: None,
            note: None,
        });
    }

    fn folder(collection: Option<&str>, folder: Option<&str>) -> Place {
        Place::Folder {
            collection: collection.map(safe_name),
            folder: folder.map(safe_name),
        }
    }

    /// Where a loose archive goes: the series it names.
    fn loose(&self, i: usize, collection: Option<&str>) -> Place {
        match self.drop.target {
            Target::Series(id) => Place::Series(id),
            _ => Self::folder(collection, Some(&series_of_file(self.name(i)))),
        }
    }

    // ---- manga and comics

    fn comics(&mut self, root: &Node) {
        let (series, top) = match &self.drop.target {
            Target::Series(id) => (Some(*id), None),
            Target::Collection(c) => (None, Some(c.clone())),
            Target::Library => (None, None),
        };
        for i in self.books(root) {
            let place = self.loose(i, top.as_deref());
            self.file(i, place);
        }
        let images = self.images(root);
        if !images.is_empty() {
            let m = self.folder_meta(root);
            let named = names::tidy(&m.title);
            let name = if !named.is_empty() {
                named
            } else if series.is_some() {
                format!("Chapter {}", self.drop.today)
            } else {
                format!("Images {}", self.drop.today)
            };
            let place = match series {
                Some(id) => Place::Series(id),
                None => {
                    let s = names::tidy(&m.series);
                    Self::folder(top.as_deref(), Some(if s.is_empty() { &name } else { &s }))
                }
            };
            let extras = self.metas(root);
            self.pack(&name, images, extras, place);
        }
        let dirs: Vec<&Node> = root.dirs.iter().collect();
        let grouped = if series.is_some() {
            Vec::new()
        } else {
            let image_dirs: Vec<&Node> = dirs
                .iter()
                .copied()
                .filter(|d| self.is_image_folder(d))
                .collect();
            stem_groups(&image_dirs)
        };
        for (name, members) in &grouped {
            for d in members {
                let place = Self::folder(top.as_deref(), Some(name));
                self.pack_folder(d, place);
            }
        }
        for d in dirs {
            if !grouped
                .iter()
                .any(|(_, m)| m.iter().any(|x| std::ptr::eq(*x, d)))
            {
                self.comics_folder(d, series, top.as_deref());
            }
        }
    }

    fn pack_folder(&mut self, d: &Node, place: Place) {
        let (title, images, extras) = (self.title_of(d), self.images(d), self.metas(d));
        self.pack(&title, images, extras, place);
    }

    fn comics_folder(&mut self, d: &Node, series: Option<i64>, collection: Option<&str>) {
        let mut image_dirs: Vec<&Node> =
            d.dirs.iter().filter(|x| self.is_image_folder(x)).collect();
        image_dirs.sort_by(|a, b| natsort::compare(&a.name, &b.name));
        let others: Vec<&Node> = d.dirs.iter().filter(|x| !self.is_image_folder(x)).collect();
        let books = self.books(d);
        let images = self.images(d);
        let as_collection = series.is_none()
            && collection.is_none()
            && (!others.is_empty()
                || (books.len() > 1
                    && !self.related(&books)
                    && images.is_empty()
                    && image_dirs.is_empty()));
        if as_collection {
            let col = d.name.as_str();
            for i in books {
                let place = self.loose(i, Some(col));
                self.file(i, place);
            }
            if !images.is_empty() {
                let place = Self::folder(Some(col), Some(&self.series_of(d)));
                self.pack_folder(d, place);
            }
            let groups = stem_groups(&image_dirs);
            for (name, members) in &groups {
                for x in members {
                    self.pack_folder(x, Self::folder(Some(col), Some(name)));
                }
            }
            for x in image_dirs {
                if !groups
                    .iter()
                    .any(|(_, m)| m.iter().any(|y| std::ptr::eq(*y, x)))
                {
                    let place = Self::folder(Some(col), Some(&self.series_of(x)));
                    self.pack_folder(x, place);
                }
            }
            for x in others {
                self.comics_folder(x, None, Some(col));
            }
            return;
        }
        let place = match series {
            Some(id) => Place::Series(id),
            None => Self::folder(collection, Some(&self.series_of(d))),
        };
        if !images.is_empty() {
            self.pack_folder(d, place.clone());
        }
        for x in image_dirs {
            self.pack_folder(x, place.clone());
        }
        for i in books {
            self.file(i, place.clone());
        }
        for x in others {
            self.comics_folder(x, series, collection);
        }
    }

    // ---- cosplay

    fn cosplay(&mut self, root: &Node) {
        let mut units = Vec::new();
        let media = self.media(root);
        if !media.is_empty() {
            let m = self.folder_meta(root);
            let t = names::tidy(&m.title);
            let name = if t.is_empty() {
                format!("Set {}", self.drop.today)
            } else {
                t
            };
            units.push(Unit {
                trail: vec![name],
                variant: None,
                what: UnitWhat::Media(media, self.metas(root)),
            });
        }
        for i in self.books(root) {
            units.push(Unit {
                trail: vec![stem_of(self.name(i)).to_owned()],
                variant: None,
                what: UnitWhat::File(i),
            });
        }
        for d in &root.dirs {
            self.cosplay_walk(d, &[], &mut units);
        }
        for u in units {
            self.cosplay_unit(u);
        }
    }

    fn cosplay_walk(&self, d: &Node, above: &[String], units: &mut Vec<Unit>) {
        let mut trail = above.to_vec();
        trail.push(d.name.clone());
        let media = self.media(d);
        if !media.is_empty() {
            units.push(Unit {
                trail: trail.clone(),
                variant: None,
                what: UnitWhat::Media(media.clone(), self.metas(d)),
            });
        }
        // A folder holding only archives is one set, each archive a variant.
        let books = self.books(d);
        let variants = d.dirs.is_empty() && media.is_empty() && books.len() > 1;
        for i in books {
            let stem = stem_of(self.name(i)).to_owned();
            let (trail, variant) = if variants {
                (trail.clone(), Some(stem))
            } else {
                let mut t = trail.clone();
                t.push(stem);
                (t, None)
            };
            units.push(Unit {
                trail,
                variant,
                what: UnitWhat::File(i),
            });
        }
        for x in &d.dirs {
            self.cosplay_walk(x, &trail, units);
        }
    }

    fn cosplay_unit(&mut self, u: Unit) {
        let first = self.plan.jobs.len();
        let mut set = where_set(&u.trail, u.variant.as_deref(), None);
        if let Target::Series(_) = self.drop.target {
            // Onto a set: what was dropped is a variant of it.
            set = SetPlace {
                circle: set.circle,
                cosplayer: None,
                set: String::new(),
                variant: u.variant.clone().or_else(|| u.trail.last().cloned()),
            };
        }
        self.cosplay_jobs(u);
        for j in &mut self.plan.jobs[first..] {
            j.set = Some(set.clone());
        }
    }

    fn cosplay_jobs(&mut self, u: Unit) {
        let place = match &self.drop.target {
            Target::Series(id) => Place::Series(*id),
            Target::Collection(c) => {
                let w = where_set(&u.trail, u.variant.as_deref(), Some(c));
                Self::folder(w.cosplayer.as_deref(), Some(&w.set))
            }
            Target::Library => {
                let w = where_set(&u.trail, u.variant.as_deref(), None);
                Self::folder(w.cosplayer.as_deref(), Some(&w.set))
            }
        };
        match u.what {
            UnitWhat::File(i) => self.file(i, place),
            UnitWhat::Media(media, extras) => {
                if media.iter().any(|&i| self.sorts[i] == Sort::Image) {
                    let name = self
                        .title_override(&extras)
                        .unwrap_or_else(|| u.trail.last().cloned().unwrap_or_default());
                    self.pack(&name, media, extras, place);
                } else {
                    // Videos alone stay files in the set's folder; M10 shows them.
                    for i in media {
                        self.file(i, place.clone());
                    }
                }
            }
        }
    }

    /// The title the metadata files among `extras` give.
    fn title_override(&self, extras: &[usize]) -> Option<String> {
        let mut layers: Vec<(usize, &Meta)> = extras
            .iter()
            .filter_map(|i| Some((meta::layer_of(self.name(*i))?, self.drop.meta.get(i)?)))
            .collect();
        layers.sort_by_key(|(layer, _)| *layer);
        let m = layers
            .into_iter()
            .fold(Meta::default(), |weak, (_, strong)| {
                meta::merge(weak, strong.clone())
            });
        let t = names::tidy(&m.title);
        (!t.is_empty()).then_some(t)
    }
}

/// A set in a cosplay drop: the folders down to it (the last is its own
/// name, or its archive's), and what it holds.
struct Unit {
    trail: Vec<String>,
    /// An archive in a folder of archives: a variant of the folder's set.
    variant: Option<String>,
    what: UnitWhat,
}

enum UnitWhat {
    File(usize),
    Media(Vec<usize>, Vec<usize>),
}

/// Where a cosplay set goes, read from the folders down to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetPlace {
    pub circle: Option<String>,
    pub cosplayer: Option<String>,
    pub set: String,
    pub variant: Option<String>,
}

/// Reads a set's place from the folders down to it, outermost first
/// (`Mirelle / Starfall Bunny / Normal`). The first name that says
/// `Cosplayer - Set` decides, and what follows it is the variant; else the
/// last three folders are cosplayer, set and variant, two are cosplayer and
/// set, and one is a set with no cosplayer. Dropped on a cosplayer, that
/// name comes first. `variant`: an archive in a folder of archives.
pub fn where_set(trail: &[String], variant: Option<&str>, cosplayer: Option<&str>) -> SetPlace {
    let t: Vec<String> = cosplayer
        .map(str::to_owned)
        .into_iter()
        .chain(trail.iter().cloned())
        .collect();
    let forced_variant = variant.map(str::to_owned);
    let skip = usize::from(cosplayer.is_some());
    for (k, x) in t.iter().enumerate().skip(skip) {
        let n = names::cosplay_name(x);
        if !n.cosplayer.is_empty() && !n.set.is_empty() {
            let tail = t[k + 1..].last().cloned();
            return SetPlace {
                circle: (!n.circle.is_empty()).then_some(n.circle),
                cosplayer: Some(n.cosplayer),
                set: n.set,
                variant: forced_variant
                    .or(tail)
                    .or((!n.variant.is_empty()).then_some(n.variant)),
            };
        }
    }
    let n = t.len();
    let (cosplayer, set, variant) = match n {
        0 => (None, "Untitled".to_owned(), None),
        1 => (None, t[0].clone(), None),
        2 => (Some(t[0].clone()), t[1].clone(), None),
        _ => (
            Some(t[n - 3].clone()),
            t[n - 2].clone(),
            Some(t[n - 1].clone()),
        ),
    };
    SetPlace {
        circle: None,
        cosplayer,
        set,
        variant: forced_variant.or(variant),
    }
}

/// Sibling folders `Name 1`, `Name 2` (or `Name Ch.1`, `Name Ch.2`), with a
/// plain `Name` as part 1, are one series `Name`: `(name, folders in order)`.
fn stem_groups<'n>(dirs: &[&'n Node]) -> Vec<(String, Vec<&'n Node>)> {
    // (lower-case key, name, folders with their numbers)
    type Group<'n> = (String, String, Vec<(&'n Node, Option<f64>)>);
    let mut groups: Vec<Group<'n>> = Vec::new();
    for d in dirs {
        let (name, number) = series_and_number(&d.name);
        if number.is_some() && !name.is_empty() {
            let key = name.to_lowercase();
            match groups.iter_mut().find(|g| g.0 == key) {
                Some(g) => g.2.push((d, number)),
                None => groups.push((key, name, vec![(d, number)])),
            }
        }
    }
    for d in dirs {
        let (name, number) = series_and_number(&d.name);
        if number.is_none()
            && let Some(g) = groups.iter_mut().find(|g| g.0 == name.to_lowercase())
        {
            g.2.push((d, None));
        }
    }
    groups
        .into_iter()
        .filter(|g| g.2.len() >= 2)
        .map(|(_, name, mut members)| {
            members.sort_by(|a, b| match (a.1, b.1) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(x), Some(y)) => x.total_cmp(&y),
            });
            (name, members.into_iter().map(|(d, _)| d).collect())
        })
        .collect()
}

/// Plans a drop.
pub fn plan(drop: &Drop) -> Plan {
    let sorts: Vec<Sort> = drop
        .items
        .iter()
        .map(|it| sort_of(file_name(&it.path)))
        .collect();
    let mut root = Node::default();
    let mut planner = Planner {
        drop,
        sorts,
        plan: Plan::default(),
    };
    for (i, it) in drop.items.iter().enumerate() {
        let parts: Vec<&str> = it.path.split('/').filter(|p| !p.is_empty()).collect();
        if parts.is_empty() || is_junk(&it.path) || parts.iter().any(|p| *p == "." || *p == "..") {
            planner.plan.ignored.push(i);
            continue;
        }
        match planner.sorts[i] {
            Sort::Rar => {
                planner
                    .plan
                    .refused
                    .push((i, "CBR is read from M11 on: convert it to CBZ first".into()));
                continue;
            }
            Sort::Video if !drop.cosplay => {
                planner
                    .plan
                    .refused
                    .push((i, "videos go in cosplay libraries".into()));
                continue;
            }
            Sort::Other => {
                planner.plan.ignored.push(i);
                continue;
            }
            _ => {}
        }
        let mut node = &mut root;
        for p in &parts[..parts.len() - 1] {
            node = node.dir(p);
        }
        node.files.push(i);
    }
    root.sort();
    if drop.cosplay {
        planner.cosplay(&root);
    } else {
        planner.comics(&root);
    }
    planner.plan
}

/// What the library already knows about the series (or cosplay set) a
/// planned file lands in. Default: nothing, a new series.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Known {
    /// The series' (or set's) name, when it exists.
    pub series: Option<String>,
    /// Its most common author (or the set's cosplayer), circle and language.
    pub author: Option<String>,
    pub circle: Option<String>,
    pub language: Option<String>,
    /// The numbers its books have, for the padding.
    pub numbers: Vec<(Option<names::Kind>, f64)>,
    pub books: usize,
}

/// The extension a file lands with: CB7 and 7z become CBZ.
fn landing_extension(name: &str) -> String {
    let ext = extension(name).unwrap_or("cbz").to_lowercase();
    if ext == "cb7" || ext == "7z" {
        "cbz".to_owned()
    } else {
        ext
    }
}

impl Job {
    /// The name it lands with: the standard one, or as dropped (CB7 as CBZ).
    pub fn landing_name(&self, standard: bool) -> String {
        match (&self.to, standard) {
            (Some(to), true) => to.clone(),
            _ => format!("{}.{}", stem_of(&self.name), landing_extension(&self.name)),
        }
    }
}

/// Gives every planned file its name under the naming standard
/// (docs/naming.md), or a note saying why it keeps the one it was dropped
/// with. `known` tells what the library has where a file lands.
pub fn name_to_standard(plan: &mut Plan, drop: &Drop, known: &dyn Fn(&Place) -> Known) {
    let mut groups: Vec<(Place, Vec<usize>)> = Vec::new();
    for (k, j) in plan.jobs.iter().enumerate() {
        match groups.iter_mut().find(|g| g.0 == j.place) {
            Some(g) => g.1.push(k),
            None => groups.push((j.place.clone(), vec![k])),
        }
    }
    for (place, members) in groups {
        let k = known(&place);
        if drop.cosplay {
            for m in members {
                cosplay_standard(&mut plan.jobs[m], &k);
            }
        } else {
            comics_standard(plan, drop, &place, &members, &k);
        }
    }
}

fn media_kind(j: &Job) -> bool {
    matches!(j.what, What::File(_)) && has_extension(&j.name, &VIDEO_EXTENSIONS)
}

fn cosplay_standard(j: &mut Job, k: &Known) {
    if media_kind(j) {
        return; // videos keep their names: they are a set's files, not the set
    }
    let Some(s) = &j.set else { return };
    let cosplayer = s.cosplayer.clone().or_else(|| k.author.clone());
    let set = if s.set.is_empty() {
        k.series.clone().unwrap_or_default()
    } else {
        s.set.clone()
    };
    let Some(cosplayer) = cosplayer.filter(|c| !c.is_empty()) else {
        j.note = Some("no cosplayer in the name: it goes to the fix-up queue".into());
        return;
    };
    let mut name = String::new();
    if let Some(c) = s.circle.as_ref().or(k.circle.as_ref()) {
        name.push_str(&format!("[{}] ", naming::clean(c)));
    }
    name.push_str(&format!(
        "{} - {}",
        naming::clean(&cosplayer),
        naming::clean(&set)
    ));
    if let Some(v) = &s.variant
        && !v.is_empty()
        && !v.eq_ignore_ascii_case(&set)
    {
        name.push_str(&format!(" - {}", naming::clean(v)));
    }
    j.to = Some(format!(
        "{}.{}",
        safe_name(&name),
        landing_extension(&j.name)
    ));
}

fn comics_standard(plan: &mut Plan, drop: &Drop, place: &Place, members: &[usize], k: &Known) {
    struct Read {
        stem: String,
        meta: Meta,
        name: names::BookName,
    }
    let reads: Vec<Read> = members
        .iter()
        .map(|&m| {
            let j = &plan.jobs[m];
            let meta = match &j.what {
                What::Pack { extras, .. } => {
                    let mut layers: Vec<(usize, &Meta)> = extras
                        .iter()
                        .filter_map(|i| {
                            Some((meta::layer_of(&drop.items[*i].path)?, drop.meta.get(i)?))
                        })
                        .collect();
                    layers.sort_by_key(|(l, _)| *l);
                    layers
                        .into_iter()
                        .fold(Meta::default(), |w, (_, s)| meta::merge(w, s.clone()))
                }
                What::File(_) => Meta::default(),
            };
            let stem = stem_of(&j.name).to_owned();
            let name = names::book_name(&stem);
            Read { stem, meta, name }
        })
        .collect();
    let number_of = |r: &Read| -> Option<(Option<names::Kind>, f64)> {
        let from_meta = r.meta.number.trim().parse::<f64>().ok();
        match (from_meta, r.name.number) {
            (Some(n), _) => Some((r.name.kind.or(Some(names::Kind::Chapter)), n)),
            (None, Some(n)) => Some((r.name.kind, n)),
            (None, None) => None,
        }
    };
    let series = match (place, &k.series) {
        (_, Some(s)) => s.clone(),
        (
            Place::Folder {
                folder: Some(f), ..
            },
            None,
        ) => f.clone(),
        _ => String::new(),
    };
    let author = naming::most_common(reads.iter().filter_map(|r| {
        r.meta
            .authors
            .first()
            .cloned()
            .or_else(|| r.name.author.clone())
    }))
    .or_else(|| k.author.clone());
    let circle = naming::most_common(reads.iter().filter_map(|r| {
        r.meta
            .publishers
            .first()
            .cloned()
            .or_else(|| r.name.publisher.clone())
    }))
    .or_else(|| k.circle.clone());
    let language = naming::most_common(reads.iter().filter_map(|r| {
        naming::language_code(&r.meta.language).or_else(|| naming::language_of(&r.stem))
    }))
    .or_else(|| k.language.clone());
    let widths = naming::Widths::of(
        k.numbers
            .iter()
            .copied()
            .chain(reads.iter().filter_map(number_of)),
    );
    let several = members.len() + k.books > 1;
    for (r, &m) in reads.iter().zip(members) {
        let j = &mut plan.jobs[m];
        let number = number_of(r);
        if series.is_empty() {
            j.note = Some("no series name".into());
            continue;
        }
        if number.is_none() && several {
            j.note = Some("no number, and not the series' only book".into());
            continue;
        }
        let parts = naming::Parts {
            event: naming::event_of(&r.stem),
            circle: r
                .meta
                .publishers
                .first()
                .cloned()
                .or_else(|| r.name.publisher.clone())
                .or_else(|| circle.clone())
                .map(|x| naming::clean(&x)),
            author: r
                .meta
                .authors
                .first()
                .cloned()
                .or_else(|| r.name.author.clone())
                .or_else(|| author.clone())
                .map(|x| naming::clean(&x)),
            series: naming::clean(&series),
            kind: number.and_then(|n| n.0),
            number: number.map(|n| n.1),
            language: naming::language_code(&r.meta.language)
                .or_else(|| naming::language_of(&r.stem))
                .or_else(|| language.clone()),
        };
        let name = naming::build(&parts, widths);
        j.to = Some(format!(
            "{}.{}",
            safe_name(&name),
            landing_extension(&j.name)
        ));
    }
    // Two files that would get one name keep theirs.
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for &m in members {
        if let Some(to) = &plan.jobs[m].to {
            *seen.entry(to.to_lowercase()).or_default() += 1;
        }
    }
    for &m in members {
        let j = &mut plan.jobs[m];
        if let Some(to) = &j.to
            && seen[&to.to_lowercase()] > 1
        {
            j.note = Some(format!("another file would also be {to}"));
            j.to = None;
        }
    }
}

impl Drop {
    /// Reads a drop sent by the browser:
    /// `{"items": [{"path", "size"}…], "meta": {"3": "<text>"}, "today"}`.
    /// The target and library kind come from the page it was dropped on.
    pub fn from_json(v: &Value, target: Target, cosplay: bool) -> Result<Drop, String> {
        let items = v.get("items").and_then(Value::as_array).ok_or("no items")?;
        if items.len() > MAX_ITEMS {
            return Err(format!("a drop holds at most {MAX_ITEMS} files"));
        }
        let items = items
            .iter()
            .map(|it| {
                let path = it
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or("an item has no path")?;
                if path.len() > 4096 {
                    return Err("a path is too long");
                }
                Ok(Item {
                    path: path.to_owned(),
                    size: it.get("size").and_then(Value::as_u64).unwrap_or(0),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut parsed = BTreeMap::new();
        if let Some(m) = v.get("meta").and_then(Value::as_object) {
            for (k, text) in m {
                let (Ok(i), Some(text)) = (k.parse::<usize>(), text.as_str()) else {
                    continue;
                };
                let Some(item) = items.get(i) else { continue };
                if text.len() > 1 << 20 {
                    continue;
                }
                if let Some(layer) = meta::layer_of(&item.path) {
                    parsed.insert(i, meta::read(&[(layer, text.as_bytes().to_vec())]));
                }
            }
        }
        let today = v
            .get("today")
            .and_then(Value::as_str)
            .filter(|t| t.len() == 10 && t.bytes().all(|b| b.is_ascii_digit() || b == b'-'))
            .unwrap_or("today")
            .to_owned();
        Ok(Drop {
            items,
            meta: parsed,
            target,
            cosplay,
            today,
        })
    }
}

impl Place {
    pub fn to_json(&self) -> Value {
        match self {
            Place::Series(id) => json!({ "series": id }),
            Place::Folder { collection, folder } => {
                let mut m = serde_json::Map::new();
                if let Some(c) = collection {
                    m.insert("collection".into(), c.clone().into());
                }
                if let Some(f) = folder {
                    m.insert("folder".into(), f.clone().into());
                }
                Value::Object(m)
            }
        }
    }
}

impl Job {
    /// The job as the browser and the golden set read it.
    pub fn to_json(&self) -> Value {
        let mut v = self.place.to_json();
        let m = v.as_object_mut().expect("an object");
        match &self.what {
            What::File(i) => {
                m.insert("kind".into(), "file".into());
                m.insert("file".into(), (*i).into());
            }
            What::Pack { pages, extras } => {
                m.insert("kind".into(), "pack".into());
                m.insert("pages".into(), json!(pages));
                m.insert("extras".into(), json!(extras));
            }
        }
        m.insert("name".into(), self.name.clone().into());
        m.insert("size".into(), self.size.into());
        if let Some(to) = &self.to {
            m.insert("to".into(), to.clone().into());
        }
        if let Some(note) = &self.note {
            m.insert("note".into(), note.clone().into());
        }
        v
    }
}

impl Plan {
    pub fn to_json(&self) -> Value {
        json!({
            "jobs": self.jobs.iter().map(Job::to_json).collect::<Vec<_>>(),
            "refused": self.refused.iter().map(|(i, why)| json!({"file": i, "why": why})).collect::<Vec<_>>(),
            "ignored": self.ignored,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names() {
        assert_eq!(safe_name("Star/fall: Zero?"), "Star fall Zero");
        assert_eq!(safe_name("..hidden"), "hidden");
        assert_eq!(safe_name("   "), "Untitled");
        assert_eq!(safe_name(&"a".repeat(400)).chars().count(), MAX_NAME);
    }

    #[test]
    fn sorts() {
        assert_eq!(sort_of("001.JPG"), Sort::Image);
        assert_eq!(sort_of("Starfall 01.cb7"), Sort::Book);
        assert_eq!(sort_of("old.cbr"), Sort::Rar);
        assert_eq!(sort_of("ComicInfo.xml"), Sort::Meta);
        assert_eq!(sort_of("clip.MOV"), Sort::Video);
        assert_eq!(sort_of("notes.txt"), Sort::Other);
        assert!(is_junk("__MACOSX/x/._001.jpg"));
        assert!(is_junk("a/Thumbs.db"));
    }
}
