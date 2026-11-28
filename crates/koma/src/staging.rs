//! Where uploads wait until the worker files them: `<data>/uploads`, written
//! only by `koma serve` (docs/design/README.md, "Uploads"). Mount a folder on
//! the library's drive there and filing an upload is a rename.
//!
//! Each upload is two files: `<id>.part`, the bytes so far, and `<id>.json`,
//! what it is and whose. A file upload is sent in pieces, each at the offset
//! the server has (the `.part` file's length), so after a dropped connection
//! the browser asks where the server got to and carries on from there. A
//! pack is a stored ZIP built on the server as its images arrive, one image
//! at a time: an image cut off on the way is dropped and sent again.
//!
//! Nothing here trusts the browser: names are checked, sizes are capped, and
//! an upload is seen only by the account that started it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};

use crate::archive::crc32::Crc32;
use crate::archive::zipout::{self, Member};
use crate::drops;

/// Largest file one upload may be.
pub const MAX_FILE: u64 = 64 * 1024 * 1024 * 1024;
/// Largest image (or metadata file) in a pack, as for pages at ingest.
pub const MAX_PAGE: u64 = 256 * 1024 * 1024;
/// Most images in one pack.
pub const MAX_PAGES: usize = 50_000;
/// An upload untouched this long is swept away.
pub const STALE: Duration = Duration::from_secs(24 * 3600);

/// What an upload is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One file, sent as it is.
    File,
    /// Images packed into a stored ZIP as they arrive.
    Pack,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::File => "file",
            Kind::Pack => "pack",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        match s {
            "file" => Some(Kind::File),
            "pack" => Some(Kind::Pack),
            _ => None,
        }
    }
}

/// An upload's description, kept in `<id>.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upload {
    pub id: String,
    pub owner: i64,
    /// The library folder it goes to (`manga`, `~mirelle/sketchbooks`).
    pub library: String,
    pub kind: Kind,
    /// Its name as dropped (for a pack, the archive's name).
    pub name: String,
    /// A file's size, as declared when it started.
    pub size: u64,
    /// The drop it is part of, for the upload log.
    pub batch: String,
    /// A pack's images so far, in the order they came.
    pub members: Vec<Member>,
    /// All of it has arrived.
    pub done: bool,
}

#[derive(Debug)]
pub enum Error {
    NotFound,
    /// Not the offset the server has: send from this one.
    Offset(u64),
    /// Already in the pack.
    Duplicate,
    /// Being written by another request: try again in a moment.
    Busy,
    /// Something the browser sent that cannot be taken, and why.
    Refused(String),
    /// The body ended early; this much is kept.
    Cut(u64),
    Io(io::Error),
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "no such upload"),
            Error::Offset(n) => write!(f, "the server has {n} bytes"),
            Error::Duplicate => write!(f, "already in this pack"),
            Error::Busy => write!(f, "being written: try again"),
            Error::Refused(why) => write!(f, "{why}"),
            Error::Cut(n) => write!(f, "cut off after {n} bytes"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

/// An upload id: 32 hex digits.
fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn new_id() -> String {
    crate::accounts::random()[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The name a pack member gets: its base name, if it is an image, a video or
/// a metadata file and can be a file name.
pub fn member_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next()?.trim();
    let ok = !base.is_empty()
        && base.len() <= 255
        && !base.starts_with('.')
        && !base.chars().any(|c| c.is_control())
        && drops::packs_member(base);
    ok.then(|| base.to_owned())
}

/// The staging folder.
#[derive(Clone, Debug)]
pub struct Staging {
    dir: PathBuf,
}

impl Staging {
    pub fn new(data: &Path) -> Self {
        Self {
            dir: data.join("uploads"),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn part(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.part"))
    }

    fn json(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Starts an upload; first sweeps away uploads left untouched for a day.
    pub fn create(
        &self,
        owner: i64,
        library: &str,
        kind: Kind,
        name: &str,
        size: u64,
        batch: &str,
    ) -> Result<Upload, Error> {
        if kind == Kind::File && size > MAX_FILE {
            return Err(Error::Refused("larger than 64 GB".into()));
        }
        if name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control) {
            return Err(Error::Refused("not a file name".into()));
        }
        fs::create_dir_all(&self.dir)?;
        self.sweep(SystemTime::now());
        let u = Upload {
            id: new_id(),
            owner,
            library: library.to_owned(),
            kind,
            name: name.to_owned(),
            size: if kind == Kind::File { size } else { 0 },
            batch: batch
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
                .take(40)
                .collect(),
            members: Vec::new(),
            done: false,
        };
        File::create(self.part(&u.id))?;
        self.save(&u)?;
        Ok(u)
    }

    /// Removes uploads untouched since `now - STALE`, and leftovers.
    pub fn sweep(&self, now: SystemTime) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for e in entries.flatten() {
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age > STALE));
            if stale {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    /// The upload `id`, if `owner` started it.
    pub fn get(&self, id: &str, owner: i64) -> Result<Upload, Error> {
        if !valid_id(id) {
            return Err(Error::NotFound);
        }
        let text = fs::read_to_string(self.json(id)).map_err(|_| Error::NotFound)?;
        let u = parse(&text).ok_or(Error::NotFound)?;
        if u.owner != owner || u.id != id {
            return Err(Error::NotFound);
        }
        Ok(u)
    }

    /// Every upload `owner` has here.
    pub fn list(&self, owner: i64) -> Vec<Upload> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let id = name.strip_suffix(".json")?;
                self.get(id, owner).ok()
            })
            .collect()
    }

    /// Bytes the server has of an upload.
    pub fn received(&self, u: &Upload) -> u64 {
        fs::metadata(self.part(&u.id)).map_or(0, |m| m.len())
    }

    /// Where the bytes of a finished upload are, for the worker.
    pub fn bytes_of(&self, u: &Upload) -> PathBuf {
        self.part(&u.id)
    }

    /// Writes the description, atomically.
    fn save(&self, u: &Upload) -> io::Result<()> {
        let tmp = self.dir.join(format!("{}.json.tmp", u.id));
        fs::write(&tmp, to_json(u).to_string())?;
        fs::rename(tmp, self.json(&u.id))
    }

    /// Appends to a file upload what `body` holds, which must start at
    /// `offset`. Returns the new length; an error from `body` keeps what
    /// was written before it (the upload goes on from there).
    pub fn append(&self, u: &Upload, offset: u64, body: &mut dyn Read) -> Result<u64, Error> {
        if u.kind != Kind::File || u.done {
            return Err(Error::Refused("not a file upload in progress".into()));
        }
        let mut f = OpenOptions::new().append(true).open(self.part(&u.id))?;
        let have = f.metadata()?.len();
        if have != offset {
            return Err(Error::Offset(have));
        }
        let mut buf = vec![0u8; 256 * 1024];
        let mut len = have;
        loop {
            let n = match body.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    f.sync_data()?;
                    return Err(Error::Cut(len));
                }
            };
            if len + n as u64 > u.size {
                f.sync_data()?;
                return Err(Error::Refused(format!(
                    "more than the {} bytes it was said to be",
                    u.size
                )));
            }
            f.write_all(&buf[..n])?;
            len += n as u64;
        }
        f.sync_data()?;
        Ok(len)
    }

    /// Adds one image (or metadata file) to a pack. The image is taken
    /// whole or not at all.
    pub fn add_member(
        &self,
        u: &mut Upload,
        name: &str,
        body: &mut dyn Read,
    ) -> Result<usize, Error> {
        if u.kind != Kind::Pack || u.done {
            return Err(Error::Refused("not a pack in progress".into()));
        }
        let name = member_name(name).ok_or_else(|| {
            Error::Refused(format!("{name}: not an image, a video or a metadata file"))
        })?;
        if u.members.iter().any(|m| m.name.eq_ignore_ascii_case(&name)) {
            return Err(Error::Duplicate);
        }
        if u.members.len() >= MAX_PAGES {
            return Err(Error::Refused(format!(
                "a pack holds at most {MAX_PAGES} files"
            )));
        }
        // The image first goes to its own file: its size and CRC must be in
        // the local header before its bytes.
        let page = self.dir.join(format!("{}.page", u.id));
        let mut crc = Crc32::default();
        let mut size: u64 = 0;
        {
            let mut out = File::create(&page)?;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = match body.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => {
                        let _ = fs::remove_file(&page);
                        return Err(Error::Cut(size));
                    }
                };
                size += n as u64;
                if size > MAX_PAGE {
                    let _ = fs::remove_file(&page);
                    return Err(Error::Refused(format!("{name}: larger than 256 MB")));
                }
                crc.update(&buf[..n]);
                out.write_all(&buf[..n])?;
            }
        }
        let crc = crc.finish();
        // Whatever follows the last member taken is from an image cut off
        // before: written over.
        let end = u.members.last().map_or(0, |m| {
            m.offset + 30 + m.name.len() as u64 + u64::from(m.size)
        });
        let zip = OpenOptions::new().write(true).open(self.part(&u.id))?;
        zip.set_len(end)?;
        let mut zip = OpenOptions::new().append(true).open(self.part(&u.id))?;
        zip.write_all(&zipout::local_header(&name, crc, size as u32))?;
        io::copy(&mut File::open(&page)?, &mut zip)?;
        zip.sync_data()?;
        let _ = fs::remove_file(&page);
        u.members.push(Member {
            name,
            crc,
            size: size as u32,
            offset: end,
        });
        self.save(u)?;
        Ok(u.members.len())
    }

    /// Marks an upload as whole: a file must have all its bytes; a pack gets
    /// its central directory and must hold at least one image.
    pub fn finish(&self, u: &mut Upload) -> Result<(), Error> {
        if u.done {
            return Ok(());
        }
        match u.kind {
            Kind::File => {
                let have = self.received(u);
                if have != u.size {
                    return Err(Error::Offset(have));
                }
            }
            Kind::Pack => {
                if !u
                    .members
                    .iter()
                    .any(|m| crate::archive::is_page_name(&m.name))
                {
                    return Err(Error::Refused("no images in it".into()));
                }
                let end = u.members.last().map_or(0, |m| {
                    m.offset + 30 + m.name.len() as u64 + u64::from(m.size)
                });
                let f = OpenOptions::new().write(true).open(self.part(&u.id))?;
                f.set_len(end)?;
                let mut f = OpenOptions::new().append(true).open(self.part(&u.id))?;
                f.write_all(&zipout::central_directory(&u.members, end, 0xFFFF_FFFF))?;
                f.sync_data()?;
                u.size = self.received(u);
            }
        }
        u.done = true;
        self.save(u)?;
        Ok(())
    }

    /// Removes an upload.
    pub fn remove(&self, u: &Upload) {
        for p in [
            self.part(&u.id),
            self.json(&u.id),
            self.dir.join(format!("{}.page", u.id)),
        ] {
            let _ = fs::remove_file(p);
        }
    }
}

pub fn to_json(u: &Upload) -> Value {
    json!({
        "id": u.id,
        "owner": u.owner,
        "library": u.library,
        "kind": u.kind.as_str(),
        "name": u.name,
        "size": u.size,
        "batch": u.batch,
        "done": u.done,
        "members": u.members.iter().map(|m| json!([m.name, m.crc, m.size, m.offset])).collect::<Vec<_>>(),
    })
}

fn parse(text: &str) -> Option<Upload> {
    let v: Value = serde_json::from_str(text).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let members = v
        .get("members")?
        .as_array()?
        .iter()
        .map(|m| {
            Some(Member {
                name: m.get(0)?.as_str()?.to_owned(),
                crc: u32::try_from(m.get(1)?.as_u64()?).ok()?,
                size: u32::try_from(m.get(2)?.as_u64()?).ok()?,
                offset: m.get(3)?.as_u64()?,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Upload {
        id: s("id")?,
        owner: v.get("owner")?.as_i64()?,
        library: s("library")?,
        kind: Kind::parse(&s("kind")?)?,
        name: s("name")?,
        size: v.get("size")?.as_u64()?,
        batch: s("batch").unwrap_or_default(),
        members,
        done: v.get("done")?.as_bool()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive::zip;

    struct Breaks<'a>(&'a [u8], usize);

    impl Read for Breaks<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.1 == 0 {
                return Err(io::ErrorKind::ConnectionReset.into());
            }
            let n = buf.len().min(self.1).min(self.0.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            self.1 -= n;
            Ok(n)
        }
    }

    fn staging(name: &str) -> Staging {
        let dir = std::env::temp_dir().join(format!("koma-staging-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Staging::new(&dir)
    }

    #[test]
    fn a_file_goes_on_where_it_stopped() {
        let s = staging("file");
        let data: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut u = s
            .create(
                1,
                "manga",
                Kind::File,
                "Starfall 01.cbz",
                data.len() as u64,
                "b1",
            )
            .unwrap();
        assert!(matches!(
            s.append(&u, 0, &mut Breaks(&data, 40_000)),
            Err(Error::Cut(40_000))
        ));
        assert!(matches!(
            s.append(&u, 0, &mut &data[..]),
            Err(Error::Offset(40_000))
        ));
        assert_eq!(s.append(&u, 40_000, &mut &data[40_000..]).unwrap(), 100_000);
        s.finish(&mut u).unwrap();
        assert_eq!(fs::read(s.bytes_of(&u)).unwrap(), data);
        assert!(
            matches!(s.get(&u.id, 2), Err(Error::NotFound)),
            "another account"
        );
        assert!(s.get(&u.id, 1).unwrap().done);
    }

    #[test]
    fn a_file_cannot_grow_past_its_size() {
        let s = staging("size");
        let u = s.create(1, "manga", Kind::File, "a.cbz", 10, "").unwrap();
        assert!(matches!(
            s.append(&u, 0, &mut &[0u8; 11][..]),
            Err(Error::Refused(_))
        ));
        assert!(
            s.create(1, "manga", Kind::File, "big.cbz", MAX_FILE + 1, "")
                .is_err()
        );
    }

    #[test]
    fn a_pack_is_a_zip_koma_reads_and_a_cut_image_is_sent_again() {
        let s = staging("pack");
        let mut u = s
            .create(1, "cosplay", Kind::Pack, "Starfall Bunny.zip", 0, "")
            .unwrap();
        s.add_member(&mut u, "Set/001.jpg", &mut &b"page one"[..])
            .unwrap();
        assert!(matches!(
            s.add_member(&mut u, "002.jpg", &mut Breaks(b"page two", 3)),
            Err(Error::Cut(3))
        ));
        s.add_member(&mut u, "002.jpg", &mut &b"page two"[..])
            .unwrap();
        assert!(matches!(
            s.add_member(&mut u, "001.JPG", &mut &b"x"[..]),
            Err(Error::Duplicate)
        ));
        for bad in ["run.sh", "../.hidden.jpg", "pack.json"] {
            assert!(
                matches!(
                    s.add_member(&mut u, bad, &mut &b"x"[..]),
                    Err(Error::Refused(_))
                ),
                "{bad}"
            );
        }
        s.add_member(&mut u, "metadata.json", &mut &b"{}"[..])
            .unwrap();
        // The server restarts: the description is read back from its file.
        let mut u = s.get(&u.id, 1).unwrap();
        s.finish(&mut u).unwrap();
        let path = s.bytes_of(&u);
        let index = zip::read_file_index(&path, &zip::Limits::default()).unwrap();
        let names: Vec<&str> = index.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["001.jpg", "002.jpg", "metadata.json"]);
        assert!(index.iter().all(|e| e.method == zip::Method::Stored));
    }

    #[test]
    fn an_empty_pack_is_refused_and_old_uploads_are_swept() {
        let s = staging("sweep");
        let mut u = s
            .create(1, "cosplay", Kind::Pack, "Empty.zip", 0, "")
            .unwrap();
        s.add_member(&mut u, "metadata.json", &mut &b"{}"[..])
            .unwrap();
        assert!(matches!(s.finish(&mut u), Err(Error::Refused(_))));
        s.sweep(SystemTime::now() + STALE + Duration::from_secs(60));
        assert!(matches!(s.get(&u.id, 1), Err(Error::NotFound)));
        assert!(s.list(1).is_empty());
    }
}
