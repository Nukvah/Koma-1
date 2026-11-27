//! 7z archives (CB7) for tests, written with LZMA2.

use std::io::Cursor;

use sevenz_rust2::{ArchiveEntry, ArchiveWriter, SourceReader};

/// A 7z archive holding these files. `solid` packs them into one block, as
/// 7-Zip does by default, so a page can only be read after the ones before it.
pub fn write(files: &[(&str, &[u8])], solid: bool) -> Vec<u8> {
    let mut writer = ArchiveWriter::new(Cursor::new(Vec::new())).expect("7z writer");
    if solid {
        let entries = files
            .iter()
            .map(|(n, _)| ArchiveEntry::new_file(n))
            .collect();
        let readers = files.iter().map(|(_, d)| SourceReader::new(*d)).collect();
        writer
            .push_archive_entries(entries, readers)
            .expect("7z block");
    } else {
        for (name, data) in files {
            writer
                .push_archive_entry(ArchiveEntry::new_file(name), Some(*data))
                .expect("7z entry");
        }
    }
    writer.finish().expect("7z finish").into_inner()
}
