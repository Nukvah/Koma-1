//! File-name parsing: brackets, numbers and credits on any text.
#![no_main]

use koma::names::{book_name, cosplay_name};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|name: &str| {
    let book = book_name(name);
    if let Some(n) = book.number {
        assert!(n.is_finite() && n >= 0.0);
    }
    cosplay_name(name);
});
