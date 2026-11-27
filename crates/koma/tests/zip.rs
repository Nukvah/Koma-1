//! The zip index against the made-up library and against hostile archives.

use koma::archive::zip::{self, Error, Limits, Method};
use koma::archive::{self, ReadAt};
use koma_fixtures::checksum::deflate_stored;
use koma_fixtures::zip::{Compression, ZipWriter};

fn index(bytes: &[u8]) -> Result<Vec<zip::Entry>, Error> {
    zip::read_index(bytes, bytes.len() as u64, &Limits::default())
}

fn small_zip() -> Vec<u8> {
    let mut w = ZipWriter::new();
    w.add("2.png", b"second page", Compression::Stored);
    w.add("10.png", b"tenth page", Compression::Deflated);
    w.add("notes.txt", b"not a page", Compression::Stored);
    w.finish()
}

#[test]
fn reads_the_made_up_library_in_reading_order() {
    let root = std::env::temp_dir().join(format!("koma-zip-test-{}", std::process::id()));
    let books = koma_fixtures::library::write(&root).unwrap();
    for book in &books {
        let path = root.join(&book.path);
        let entries = zip::read_file_index(&path, &Limits::default())
            .unwrap_or_else(|e| panic!("{}: {e}", book.path));
        let pages = archive::pages(&entries);
        let names: Vec<&str> = pages.iter().map(|e| e.name.as_str()).collect();
        let want: Vec<&str> = book.pages.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, want, "{}", book.path);

        let file = std::fs::File::open(&path).unwrap();
        for (entry, (_, bytes)) in pages.iter().zip(&book.pages) {
            let range = entry.data_range();
            let mut data = vec![0u8; (range.end - range.start) as usize];
            file.read_exact_at(&mut data, range.start).unwrap();
            assert_eq!(entry.uncompressed_size, bytes.len() as u64);
            assert_eq!(entry.crc32, koma::archive::crc32::checksum(bytes));
            match entry.method {
                Method::Stored => assert_eq!(&data, bytes, "{} {}", book.path, entry.name),
                Method::Deflated => assert_eq!(data, deflate_stored(bytes)),
                Method::Other(m) => panic!("unexpected method {m}"),
            }
        }
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn zip64_and_classic_give_the_same_index() {
    let build = |w: ZipWriter| {
        let mut w = w;
        w.add("a.png", b"aaaa", Compression::Stored);
        w.add("b.png", b"bbbbbb", Compression::Deflated);
        w.finish()
    };
    let classic = index(&build(ZipWriter::new())).unwrap();
    let zip64 = index(&build(ZipWriter::new().force_zip64())).unwrap();
    let summary = |entries: &[zip::Entry]| -> Vec<_> {
        entries
            .iter()
            .map(|e| {
                (
                    e.name.clone(),
                    e.method,
                    e.compressed_size,
                    e.uncompressed_size,
                    e.crc32,
                )
            })
            .collect()
    };
    assert_eq!(summary(&classic), summary(&zip64));
}

#[test]
fn unicode_path_field_wins_over_the_raw_name() {
    use koma_fixtures::zip::Name;
    let mut w = ZipWriter::new();
    let raw = b"p?gina 1.png".to_vec();
    w.add_named(
        Name::RawWithUnicodePath(raw, "página 1.png".into()),
        b"x",
        Compression::Stored,
    );
    // A field whose CRC doesn't match the raw name is ignored.
    let mut stale = ZipWriter::new();
    stale.add_named(
        Name::RawWithUnicodePath(b"other.png".to_vec(), "página 2.png".into()),
        b"y",
        Compression::Stored,
    );
    let good = w.finish();
    assert_eq!(index(&good).unwrap()[0].name, "página 1.png");
    let mut stale = stale.finish();
    // Change the raw name in both headers after the CRC was computed.
    for i in 0..stale.len() - 9 {
        if &stale[i..i + 9] == b"other.png" {
            stale[i] = b'O';
        }
    }
    assert_eq!(index(&stale).unwrap()[0].name, "Other.png");
}

#[test]
fn empty_archive_has_no_entries() {
    assert!(index(&ZipWriter::new().finish()).unwrap().is_empty());
}

#[test]
fn rejects_what_is_not_a_zip() {
    assert!(matches!(index(b""), Err(Error::NotZip)));
    assert!(matches!(
        index(&koma_fixtures::png::page(8, 8, 1)),
        Err(Error::NotZip)
    ));
}

#[test]
fn rejects_trailing_bytes_and_a_lying_comment_length() {
    let mut z = small_zip();
    z.push(0);
    assert!(matches!(index(&z), Err(Error::NotZip)));

    let mut z = small_zip();
    let n = z.len();
    z[n - 2] = 5; // comment length 5, but no comment follows
    assert!(matches!(index(&z), Err(Error::NotZip)));
}

#[test]
fn rejects_prepended_data() {
    let mut z = b"junk before the archive".to_vec();
    z.extend(small_zip());
    assert!(matches!(index(&z), Err(Error::Malformed(_))));
}

#[test]
fn enforces_limits() {
    let z = small_zip();
    let few = Limits {
        max_entries: 2,
        ..Limits::default()
    };
    assert!(matches!(
        zip::read_index(&z[..], z.len() as u64, &few),
        Err(Error::Limit(_))
    ));
    let short = Limits {
        max_name_len: 5,
        ..Limits::default()
    };
    assert!(matches!(
        zip::read_index(&z[..], z.len() as u64, &short),
        Err(Error::Limit(_))
    ));
    let tiny = Limits {
        max_central_directory: 10,
        ..Limits::default()
    };
    assert!(matches!(
        zip::read_index(&z[..], z.len() as u64, &tiny),
        Err(Error::Limit(_))
    ));
}

/// Two central entries that point at the same bytes: a classic zip bomb.
#[test]
fn rejects_overlapping_entries() {
    let mut w = ZipWriter::new();
    w.add("1.png", b"one", Compression::Stored);
    let z = w.finish();
    let eocd = z.len() - 22;
    let cd_offset = u32::from_le_bytes(z[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    let central = z[cd_offset..eocd].to_vec();

    let mut bomb = z[..cd_offset].to_vec();
    bomb.extend(&central);
    bomb.extend(&central);
    let mut end = z[eocd..].to_vec();
    end[8..10].copy_from_slice(&2u16.to_le_bytes());
    end[10..12].copy_from_slice(&2u16.to_le_bytes());
    end[12..16].copy_from_slice(&(2 * central.len() as u32).to_le_bytes());
    bomb.extend(end);
    assert!(matches!(
        index(&bomb),
        Err(Error::Malformed("entries overlap"))
    ));
}

#[test]
fn central_directory_offset_out_of_range() {
    let mut z = small_zip();
    let eocd = z.len() - 22;
    z[eocd + 16..eocd + 20].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(index(&z).is_err());
}

#[test]
fn junk_and_non_images_are_not_pages() {
    let entries = index(&small_zip()).unwrap();
    let pages: Vec<_> = archive::pages(&entries)
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(pages, ["2.png", "10.png"]);
}

#[test]
fn encrypted_entries_are_not_pages() {
    let mut z = small_zip();
    // Set the encryption flag on the first central header.
    let eocd = z.len() - 22;
    let cd = u32::from_le_bytes(z[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    z[cd + 8] |= 1;
    let entries = index(&z).unwrap();
    assert!(entries[0].encrypted);
    assert!(!archive::is_page(&entries[0]));
}

/// Whatever the bytes say, a successful index only points inside the file.
fn check_invariants(z: &[u8], entries: &[zip::Entry]) {
    for e in entries {
        assert!(e.data_range().end <= z.len() as u64);
        assert!(e.header_offset < e.data_offset);
    }
}

#[test]
fn every_truncation_is_an_error_not_a_panic() {
    for zip in [small_zip(), {
        let mut w = ZipWriter::new().force_zip64();
        w.add("1.png", b"x", Compression::Stored);
        w.finish()
    }] {
        for len in 0..zip.len() {
            assert!(index(&zip[..len]).is_err(), "prefix of {len} bytes parsed");
        }
    }
}

#[test]
fn every_corrupted_byte_is_handled() {
    for zip in [small_zip(), {
        let mut w = ZipWriter::new().force_zip64();
        w.add("1.png", b"page", Compression::Deflated);
        w.add("2.png", b"page two", Compression::Stored);
        w.finish()
    }] {
        for i in 0..zip.len() {
            for flip in [0x01, 0x80, 0xFF] {
                let mut z = zip.clone();
                z[i] ^= flip;
                if let Ok(entries) = index(&z) {
                    check_invariants(&z, &entries);
                }
            }
        }
    }
}
