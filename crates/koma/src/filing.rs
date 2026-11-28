//! The worker's side of uploads: each finished upload is checked like
//! ingest checks a book, converted where needed (CB7 to CBZ, pages
//! stored), skipped when the same bytes are already where it goes, and
//! moved into the library. [`crate::organise`] runs it as one journaled
//! change, so undo takes the books away again (to the trash).

use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};

use crate::archive::crc32::{Crc32, checksum};
use crate::archive::page::{PageError, PageLimits, copy_page};
use crate::archive::zip::{self, Entry, Method};
use crate::archive::zipout::{self, Member};
use crate::archive::{self, sevenz};

/// What a checked upload is.
#[derive(Debug, PartialEq, Eq)]
pub enum Checked {
    /// Ready to go in, as it is (or as converted, at this path).
    Ready(PathBuf),
    /// A video in a cosplay set: taken as it is, not read.
    Video,
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

/// Checks an upload at `path` that lands as `name`, converting a CB7 to a
/// stored CBZ next to it first. The error says, in a sentence, what is wrong.
pub fn check(path: &Path, dropped_name: &str) -> Result<Checked, String> {
    let ext = extension(dropped_name);
    match ext.as_str() {
        "mp4" | "webm" | "mkv" | "mov" | "m4v" => Ok(Checked::Video),
        "cb7" | "7z" => {
            let out = path.with_extension("cbz");
            convert_7z(path, &out)?;
            check_zip(&out, false)?;
            Ok(Checked::Ready(out))
        }
        "epub" => {
            check_zip(path, true)?;
            Ok(Checked::Ready(path.to_path_buf()))
        }
        "cbz" | "zip" => {
            check_zip(path, false)?;
            Ok(Checked::Ready(path.to_path_buf()))
        }
        "cbr" | "rar" => Err("CBR is read from M11 on: convert it to CBZ first".into()),
        _ => Err("not a book Koma-1 reads".into()),
    }
}

/// An archive's index and every page in it, as ingest reads them.
fn check_zip(path: &Path, epub: bool) -> Result<(), String> {
    let entries = zip::read_file_index(path, &zip::Limits::default()).map_err(|e| {
        format!("This archive's index is broken ({e}): it was cut off or written wrong")
    })?;
    if epub {
        if !entries.iter().any(|e| e.name == "META-INF/container.xml") {
            return Err("not an EPUB: it has no META-INF/container.xml".into());
        }
        return Ok(());
    }
    let pages = archive::pages(&entries);
    if pages.is_empty() {
        return Err("no pages in it".into());
    }
    let file = File::open(path).map_err(|e| e.to_string())?;
    let limits = PageLimits::default();
    for e in pages {
        match copy_page(&file, e, &limits, &mut io::sink()) {
            Ok(_) => {}
            Err(PageError::Corrupt(_)) => {
                return Err(format!(
                    "This archive is damaged: {}, {}",
                    e.name,
                    damage(&file, e)
                ));
            }
            Err(err) => return Err(format!("{}: {err}", e.name)),
        }
    }
    Ok(())
}

/// What exactly is wrong with a page whose bytes do not check out: same
/// size and another checksum means bytes changed; less means cut off.
fn damage(file: &File, e: &Entry) -> String {
    if e.method != Method::Stored {
        return "its compressed data does not unpack to what the archive says".into();
    }
    let mut crc = Crc32::default();
    let mut left = e.compressed_size;
    let mut at = e.data_offset;
    let mut buf = vec![0u8; 256 * 1024];
    let mut read = 0u64;
    while left > 0 {
        let want = (left as usize).min(buf.len());
        match std::os::unix::fs::FileExt::read_at(file, &mut buf[..want], at) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                crc.update(&buf[..n]);
                at += n as u64;
                left -= n as u64;
                read += n as u64;
            }
        }
    }
    if read < e.compressed_size {
        return format!(
            "it stops after {read} of {} bytes: the file was cut off",
            e.compressed_size
        );
    }
    format!(
        "stored checksum {:08x}, content gives {:08x} ({} bytes, same size: the bytes themselves differ)",
        e.crc32,
        crc.finish(),
        e.uncompressed_size
    )
}

/// A CB7 rewritten as a stored ZIP: its pages and metadata files, each
/// within the limits ingest uses.
fn convert_7z(from: &Path, to: &Path) -> Result<(), String> {
    let limits = PageLimits::default();
    let want = |m: &sevenz::Member| -> Option<u64> {
        if m.is_dir || archive::is_hidden(&m.name) {
            None
        } else if archive::is_page_name(&m.name) {
            Some(limits.max_bytes)
        } else if crate::meta::layer_of(&m.name).is_some() {
            Some(crate::meta::MAX_META_BYTES)
        } else {
            None
        }
    };
    let file = File::open(from).map_err(|e| e.to_string())?;
    let mut out = io::BufWriter::new(File::create(to).map_err(|e| e.to_string())?);
    let mut members: Vec<Member> = Vec::new();
    let mut at = 0u64;
    let mut names = std::collections::HashSet::new();
    let read = sevenz::read(
        BufReader::new(file),
        &sevenz::Limits::default(),
        want,
        |m, data| {
            let mut bytes = Vec::with_capacity(m.size.min(limits.max_bytes) as usize);
            data.read_to_end(&mut bytes)?;
            if bytes.len() as u64 != m.size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a member is not the size it says",
                ));
            }
            let name = m.name.trim_start_matches('/').to_owned();
            if !names.insert(name.to_lowercase()) {
                return Ok(()); // the same name twice: the first one stays
            }
            let crc = checksum(&bytes);
            out.write_all(&zipout::local_header(&name, crc, bytes.len() as u32))?;
            out.write_all(&bytes)?;
            members.push(Member {
                name: name.clone(),
                crc,
                size: bytes.len() as u32,
                offset: at,
            });
            at += 30 + name.len() as u64 + bytes.len() as u64;
            Ok(())
        },
    );
    if let Err(e) = read {
        let _ = fs::remove_file(to);
        return Err(format!("This CB7 cannot be read: {e}"));
    }
    out.write_all(&zipout::central_directory(&members, at, 0xFFFF_FFFF))
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())?;
    out.get_ref().sync_all().map_err(|e| e.to_string())?;
    Ok(())
}

/// The file in `folder` with exactly the bytes of `path`, if any: sizes
/// first, then a BLAKE3 hash of those of the same size.
pub fn same_file_in(folder: &Path, path: &Path) -> Option<String> {
    let len = fs::metadata(path).ok()?.len();
    let mut ours: Option<blake3::Hash> = None;
    for e in fs::read_dir(folder).ok()?.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if !m.is_file() || m.len() != len {
            continue;
        }
        let mine = match ours {
            Some(h) => h,
            None => {
                let h = hash(path).ok()?;
                ours = Some(h);
                h
            }
        };
        if hash(&e.path()).ok() == Some(mine) {
            return e.file_name().into_string().ok();
        }
    }
    None
}

fn hash(path: &Path) -> io::Result<blake3::Hash> {
    let mut h = blake3::Hasher::new();
    io::copy(&mut File::open(path)?, &mut h)?;
    Ok(h.finalize())
}

/// Moves `from` to `to` (a free path): a rename on one drive, else a copy
/// made safe on disk before the original goes.
pub fn put(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            // EXDEV: the uploads folder is on another drive.
            let tmp = to.with_extension("koma-part");
            let copied = (|| {
                let mut out = File::create(&tmp)?;
                io::copy(&mut File::open(from)?, &mut out)?;
                out.sync_all()?;
                fs::rename(&tmp, to)
            })();
            if copied.is_err() {
                let _ = fs::remove_file(&tmp);
            }
            copied?;
            fs::remove_file(from)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("koma-filing-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn zip_of(pages: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut members = Vec::new();
        for (name, data) in pages {
            let crc = checksum(data);
            members.push(Member {
                name: (*name).into(),
                crc,
                size: data.len() as u32,
                offset: out.len() as u64,
            });
            out.extend(zipout::local_header(name, crc, data.len() as u32));
            out.extend(*data);
        }
        let at = out.len() as u64;
        out.extend(zipout::central_directory(&members, at, 0xFFFF_FFFF));
        out
    }

    #[test]
    fn a_damaged_page_is_explained() {
        let d = dir("damaged");
        let page = vec![7u8; 5000];
        let mut bytes = zip_of(&[("001.jpg", &page), ("002.jpg", b"second page")]);
        bytes[2000] ^= 0xFF; // same size, a byte changed
        let p = d.join("Starfall 03.cbz");
        fs::write(&p, &bytes).unwrap();
        let why = check(&p, "Starfall 03.cbz").unwrap_err();
        assert!(why.contains("damaged") && why.contains("001.jpg"), "{why}");
        assert!(why.contains("the bytes themselves differ"), "{why}");

        // An index that points to the wrong places is broken, not "changed bytes".
        let mut bytes = zip_of(&[("001.jpg", &page)]);
        let end = bytes.windows(4).rposition(|w| w == b"PK\x05\x06").unwrap();
        let at = u32::from_le_bytes(bytes[end + 16..end + 20].try_into().unwrap());
        bytes[end + 16..end + 20].copy_from_slice(&(at - 16).to_le_bytes());
        fs::write(&p, &bytes).unwrap();
        let why = check(&p, "Starfall 03.cbz").unwrap_err();
        assert!(why.contains("index") && !why.contains("checksum"), "{why}");
    }

    #[test]
    fn duplicates_are_found_by_their_bytes() {
        let d = dir("dupes");
        fs::create_dir_all(d.join("series")).unwrap();
        fs::write(d.join("series/Starfall 01.cbz"), b"same bytes").unwrap();
        fs::write(d.join("series/Starfall 02.cbz"), b"other byte").unwrap();
        fs::write(d.join("up.part"), b"same bytes").unwrap();
        assert_eq!(
            same_file_in(&d.join("series"), &d.join("up.part")).as_deref(),
            Some("Starfall 01.cbz")
        );
        fs::write(d.join("up.part"), b"new bytes!").unwrap();
        assert_eq!(same_file_in(&d.join("series"), &d.join("up.part")), None);
    }

    #[test]
    fn cbr_waits_for_m11() {
        let d = dir("cbr");
        fs::write(d.join("x.part"), b"Rar!").unwrap();
        assert!(
            check(&d.join("x.part"), "old.cbr")
                .unwrap_err()
                .contains("M11")
        );
    }
}
