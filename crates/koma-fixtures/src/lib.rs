//! Made-up test material for Koma-1, written from scratch with no dependencies
//! so it shares no code with what it tests:
//!
//! - [`png`]: tiny grayscale PNG pages, each one different;
//! - [`zip`]: a zip writer with the variations real archives have (stored and
//!   deflated members, Zip64, code page 437 names, junk files);
//! - [`library`]: a whole library of made-up series, with the reading order
//!   every book is expected to have.
//!
//! Every title and name here is invented.

pub mod checksum;
pub mod library;
pub mod png;
pub mod zip;
