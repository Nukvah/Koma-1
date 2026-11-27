//! 7z (CB7) headers and decoding, with small limits so a run stays fast.
#![no_main]

use std::io::Cursor;

use koma::archive::sevenz;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let limits = sevenz::Limits {
        max_entries: 1000,
        max_memory_kb: 16 * 1024,
        max_total_bytes: 16 * 1024 * 1024,
        max_ratio: 1000,
    };
    let _ = sevenz::read(
        Cursor::new(data),
        &limits,
        |m| (!m.is_dir).then_some(1024 * 1024),
        |m, r| {
            let mut n = 0u64;
            let mut buf = [0u8; 4096];
            loop {
                let got = r.read(&mut buf)?;
                if got == 0 {
                    break;
                }
                n += got as u64;
            }
            // Never more than one byte past the limit given.
            assert!(n <= 1024 * 1024 + 1, "{}: {n} bytes", m.name);
            Ok(())
        },
    );
});
