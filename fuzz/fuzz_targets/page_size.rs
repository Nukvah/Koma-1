//! Page sizes from image headers: JPEG (EXIF), PNG, GIF, WebP, BMP, AVIF, JXL.
#![no_main]

use koma::archive::size::read_size;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some(size) = read_size(data, 0, data.len() as u64) {
        assert!(size.width > 0 && size.height > 0);
    }
});
