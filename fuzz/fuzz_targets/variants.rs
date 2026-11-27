//! Page variants: decoding a hostile page (JPEG, PNG, WebP, GIF, BMP),
//! then resizing and encoding it, must never panic or overrun a limit.
#![no_main]

use koma::variants::{Limits, make};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Small limits keep each run fast; a page over 720 wide still gets a
    // variant, so resizing and both encoders run too.
    let limits = Limits {
        max_pixels: 2_000_000,
        max_alloc: 64 * 1024 * 1024,
    };
    if let Ok(variants) = make(data, &limits) {
        for v in variants {
            assert!(v.width < 2_000_000 && !v.bytes.is_empty());
        }
    }
});
