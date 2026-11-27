//! 7z books (CB7), read with `sevenz-rust2` (pure Rust; LZMA and LZMA2).
//!
//! 7z is usually solid: a page can only be decoded after every member before
//! it in its block. So a CB7 is read once, front to back, at ingest, and the
//! wanted members (pages, metadata files) are streamed out as they come; the
//! pages go to the blob store and are served from there.
//!
//! Limits, all checked before decoding: the number of members, the decoder's
//! memory (LZMA dictionaries can ask for gigabytes), the total declared size,
//! and its ratio to the file's size. Each member is also cut at its own limit
//! while decoding, and the decoder checks every member's size and CRC.

use std::io::{self, Read, Seek, SeekFrom};

use sevenz_rust2::{ArchiveReader, EntryDecision, Password};

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub max_entries: usize,
    /// Largest decoder dictionary or model, in KiB.
    pub max_memory_kb: usize,
    /// Most bytes all members may declare together.
    pub max_total_bytes: u64,
    /// Largest ratio of declared bytes to the archive's size.
    pub max_ratio: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 50_000,
            // 7-Zip's "ultra" preset uses 64 MiB; leave room on a 1 GB board.
            max_memory_kb: 128 * 1024,
            max_total_bytes: 8 * 1024 * 1024 * 1024,
            max_ratio: 1000,
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// Not a 7z archive, or one this reader can't decode.
    Format(String),
    /// Over one of the [`Limits`], or a member over the limit it was given.
    Limit(&'static str),
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Format(why) => write!(f, "bad 7z archive: {why}"),
            Error::Limit(why) => write!(f, "7z archive over limit: {why}"),
            Error::Io(e) => write!(f, "read error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// A member, as the archive lists it. Paths use `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// Opens a 7z archive and streams the members `want` asks for to `each`, in
/// archive order. `want` returns the most bytes a member may have, or `None`
/// to skip it. `each` gets the member and a reader of its bytes; it need not
/// read them all. Returns every member listed.
pub fn read<R: Read + Seek>(
    mut source: R,
    limits: &Limits,
    want: impl Fn(&Member) -> Option<u64>,
    mut each: impl FnMut(&Member, &mut dyn Read) -> io::Result<()>,
) -> Result<Vec<Member>, Error> {
    let file_len = source.seek(SeekFrom::End(0))?;
    source.seek(SeekFrom::Start(0))?;
    let mut reader =
        ArchiveReader::new_with_memory_limit(source, Password::empty(), limits.max_memory_kb)
            .map_err(format_error)?;
    reader.set_thread_count(1);

    let files = &reader.archive().files;
    if files.len() > limits.max_entries {
        return Err(Error::Limit("too many members"));
    }
    let members: Vec<Member> = files
        .iter()
        .map(|f| Member {
            name: f.name.replace('\\', "/"),
            size: f.size,
            is_dir: f.is_directory,
        })
        .collect();
    let total = members
        .iter()
        .try_fold(0u64, |sum, m| sum.checked_add(m.size))
        .ok_or(Error::Limit("total size overflows"))?;
    if total > limits.max_total_bytes {
        return Err(Error::Limit("total size too large"));
    }
    if total / file_len.max(1) > limits.max_ratio {
        return Err(Error::Limit("compression ratio too high"));
    }
    for m in &members {
        if want(m).is_some_and(|max| m.size > max) {
            return Err(Error::Limit("member too large"));
        }
    }
    if !members.iter().any(|m| want(m).is_some()) {
        return Ok(members);
    }

    let mut failed = None;
    reader
        .for_each_entries_with_decision(|entry, data| {
            let member = Member {
                name: entry.name.replace('\\', "/"),
                size: entry.size,
                is_dir: entry.is_directory,
            };
            if let Some(max) = want(&member) {
                // One byte over, so a member longer than it says is noticed.
                let mut limited = data.take(max.saturating_add(1));
                if let Err(e) = each(&member, &mut limited) {
                    failed = Some(e);
                    return Ok(EntryDecision::Stop);
                }
            }
            // Continue drains what is left and checks size and CRC.
            Ok(EntryDecision::Continue)
        })
        .map_err(format_error)?;
    if let Some(e) = failed {
        return Err(Error::Io(e));
    }
    Ok(members)
}

fn format_error(e: sevenz_rust2::Error) -> Error {
    match e {
        sevenz_rust2::Error::Io(e, _) if e.kind() != io::ErrorKind::InvalidData => Error::Io(e),
        e => Error::Format(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn names(bytes: &[u8], limits: &Limits) -> Result<Vec<(String, Vec<u8>)>, Error> {
        let mut got = Vec::new();
        read(
            Cursor::new(bytes),
            limits,
            |m| (!m.is_dir).then_some(1 << 20),
            |m, r| {
                let mut buf = Vec::new();
                r.read_to_end(&mut buf)?;
                got.push((m.name.clone(), buf));
                Ok(())
            },
        )?;
        Ok(got)
    }

    #[test]
    fn reads_solid_and_non_solid_archives() {
        let files: [(&str, &[u8]); 2] = [("b/2.png", b"two"), ("a/1.png", b"one!")];
        for solid in [true, false] {
            let bytes = koma_fixtures::sevenz::write(&files, solid);
            let got = names(&bytes, &Limits::default()).unwrap();
            assert_eq!(
                got,
                [
                    ("b/2.png".to_owned(), b"two".to_vec()),
                    ("a/1.png".to_owned(), b"one!".to_vec())
                ]
            );
        }
    }

    #[test]
    fn limits_are_checked_before_decoding() {
        let big = vec![0u8; 100_000];
        let bytes = koma_fixtures::sevenz::write(&[("1.png", &big)], true);
        let ratio = Limits {
            max_ratio: 10,
            ..Limits::default()
        };
        assert!(matches!(names(&bytes, &ratio), Err(Error::Limit(_))));
        let total = Limits {
            max_total_bytes: 1000,
            ..Limits::default()
        };
        assert!(matches!(names(&bytes, &total), Err(Error::Limit(_))));
        let entries = Limits {
            max_entries: 0,
            ..Limits::default()
        };
        assert!(matches!(names(&bytes, &entries), Err(Error::Limit(_))));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(names(b"7z\xbc\xaf\x27\x1c\0\x04garbage", &Limits::default()).is_err());
        assert!(names(b"", &Limits::default()).is_err());
        assert!(names(b"PK\x03\x04", &Limits::default()).is_err());
    }
}
