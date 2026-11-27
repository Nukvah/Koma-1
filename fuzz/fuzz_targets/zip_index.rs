//! The ZIP index: central directory, Zip64, names in any encoding.
#![no_main]

use koma::archive::{self, zip};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = zip::Limits {
        max_entries: 1000,
        ..zip::Limits::default()
    };
    if let Ok(entries) = zip::read_index(data, data.len() as u64, &limits) {
        for entry in archive::pages(&entries) {
            // Every range handed out must lie inside the archive.
            let range = entry.data_range();
            assert!(range.start <= range.end && range.end <= data.len() as u64);
        }
    }
});
