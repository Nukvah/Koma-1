//! Reading archives without extracting them.
//!
//! Archives are hostile input: every length and offset read from a file is
//! checked against the file before it is used, and nothing here panics on
//! malformed data.

pub mod crc32;
pub mod page;
pub mod size;
pub mod zip;

use std::io;

/// Positioned reads, so an index can be built from a file or from bytes in
/// memory (tests, fuzzing) without seeking.
pub trait ReadAt {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()>;
}

impl ReadAt for [u8] {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let start = usize::try_from(offset).ok();
        let range = start.and_then(|s| Some(s..s.checked_add(buf.len())?));
        match range.and_then(|r| self.get(r)) {
            Some(src) => {
                buf.copy_from_slice(src);
                Ok(())
            }
            None => Err(io::ErrorKind::UnexpectedEof.into()),
        }
    }
}

impl ReadAt for Vec<u8> {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        self.as_slice().read_exact_at(buf, offset)
    }
}

impl ReadAt for std::fs::File {
    fn read_exact_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buf, offset)
    }
}

/// File extensions treated as pages.
const PAGE_EXTENSIONS: [&str; 8] = ["jpg", "jpeg", "png", "webp", "gif", "avif", "bmp", "jxl"];

/// Whether an archive member is a page: an image, not a directory, not
/// encrypted, stored or deflated, and not hidden or macOS metadata.
pub fn is_page(entry: &zip::Entry) -> bool {
    if entry.is_dir() || entry.encrypted || matches!(entry.method, zip::Method::Other(_)) {
        return false;
    }
    let hidden = entry
        .name
        .split('/')
        .any(|part| part.starts_with('.') || part == "__MACOSX");
    !hidden && is_page_name(&entry.name)
}

/// Whether a file name has a page image's extension.
pub fn is_page_name(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, ext)| PAGE_EXTENSIONS.iter().any(|p| p.eq_ignore_ascii_case(ext)))
}

/// The pages of an archive in reading order (natural sort of their paths).
pub fn pages(entries: &[zip::Entry]) -> Vec<&zip::Entry> {
    let mut pages: Vec<_> = entries.iter().filter(|e| is_page(e)).collect();
    pages.sort_by(|a, b| crate::natsort::compare_paths(&a.name, &b.name));
    pages
}

#[cfg(test)]
mod tests {
    use super::ReadAt;

    #[test]
    fn slice_reads_are_bounds_checked() {
        let data = [1u8, 2, 3, 4];
        let mut buf = [0u8; 2];
        data.read_exact_at(&mut buf, 2).unwrap();
        assert_eq!(buf, [3, 4]);
        assert!(data.read_exact_at(&mut buf, 3).is_err());
        assert!(data.read_exact_at(&mut buf, u64::MAX).is_err());
        assert!(data.read_exact_at(&mut [], 4).is_ok());
    }
}
