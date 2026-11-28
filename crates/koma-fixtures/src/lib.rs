//! Made-up test material for Koma-1, written from scratch so it shares no code
//! with what it tests (7z aside, below):
//!
//! - [`png`]: tiny grayscale PNG pages, each one different;
//! - [`zip`]: a zip writer with the variations real archives have (stored and
//!   deflated members, Zip64, code page 437 names, junk files);
//! - [`sevenz`]: 7z archives (CB7). Writing LZMA from scratch is out of scope,
//!   so these use the same crate Koma-1 reads them with; the reading order and
//!   bytes they are checked against still come from here;
//! - [`bulk`]: large files whose bytes follow from their offset;
//! - [`library`]: a whole library of made-up series, with the reading order
//!   every book is expected to have.
//!
//! Every title and name here is invented.

pub mod bulk;
pub mod checksum;
pub mod epub;
pub mod library;
pub mod png;
pub mod sevenz;
pub mod zip;
