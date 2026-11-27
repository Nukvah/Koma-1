//! Metadata files: ComicInfo.xml and metadata.json, each layer and merged.
#![no_main]

use koma::meta;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    for parser in &meta::PARSERS {
        (parser.parse)(data);
    }
    meta::read(&[(0, data.to_vec()), (1, data.to_vec())]);
});
