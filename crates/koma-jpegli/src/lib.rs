//! JPEG encoding through jpegli, behind one safe function.
//!
//! jpegli (from libjxl) writes JPEGs any decoder reads, smaller than other
//! encoders at the same visual quality (docs/m9-progress.md has the
//! measurements). The `jpegli` crate holds the FFI; it reports libjpeg's
//! errors by unwinding, which [`encode`] catches, so a failure is an
//! [`Error`] and never a panic. That needs unwinding panics (the default):
//! with `panic = "abort"` such an error would abort.

use std::fmt;
use std::panic::{self, AssertUnwindSafe};

/// JPEG's largest width or height.
pub const MAX_DIMENSION: u32 = 65_500;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Width or height is 0 or over [`MAX_DIMENSION`].
    Dimensions,
    /// The buffer is not `width * height` bytes per channel.
    BufferSize,
    /// The quality is not between 1 and 100.
    Quality,
    /// jpegli could not encode the picture.
    Encode(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Dimensions => f.write_str("picture too large or empty for JPEG"),
            Error::BufferSize => f.write_str("pixel buffer does not match the picture size"),
            Error::Quality => f.write_str("JPEG quality out of range"),
            Error::Encode(e) => write!(f, "jpegli failed to encode the picture: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Encodes packed 8-bit pixels (`width * height` of them, rows top to bottom,
/// no padding; one byte each when `gray`, else three, RGB) as a progressive
/// JPEG with optimised Huffman tables, at `quality` (1 to 100, as libjpeg's).
pub fn encode(
    pixels: &[u8],
    width: u32,
    height: u32,
    gray: bool,
    quality: f32,
) -> Result<Vec<u8>, Error> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Error::Dimensions);
    }
    let channels = if gray { 1 } else { 3 };
    if pixels.len() as u64 != u64::from(width) * u64::from(height) * channels {
        return Err(Error::BufferSize);
    }
    if !(1.0..=100.0).contains(&quality) {
        return Err(Error::Quality);
    }
    let run = || -> std::io::Result<Vec<u8>> {
        let space = if gray {
            jpegli::ColorSpace::JCS_GRAYSCALE
        } else {
            jpegli::ColorSpace::JCS_RGB
        };
        let mut c = jpegli::Compress::new(space);
        c.set_size(width as usize, height as usize);
        c.set_quality(quality);
        c.set_optimize_coding(true);
        c.set_progressive_mode();
        let mut started = c.start_compress(Vec::new())?;
        started.write_scanlines(pixels)?;
        started.finish()
    };
    // Nothing is shared with the closure but `pixels`, which it only reads.
    match panic::catch_unwind(AssertUnwindSafe(run)) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(e)) => Err(Error::Encode(e.to_string())),
        Err(payload) => Err(Error::Encode(
            payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                .unwrap_or_else(|| "libjpeg error".to_owned()),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| [(i % w) as u8, (i / w) as u8, 90])
            .collect()
    }

    #[test]
    fn writes_a_progressive_jpeg() {
        let j = encode(&gradient(300, 200), 300, 200, false, 85.0).unwrap();
        assert!(j.starts_with(&[0xff, 0xd8]) && j.ends_with(&[0xff, 0xd9]));
        // SOF2: progressive.
        assert!(j.windows(2).any(|w| w == [0xff, 0xc2]));
    }

    #[test]
    fn writes_gray_with_one_component() {
        let px = vec![128u8; 64 * 48];
        let j = encode(&px, 64, 48, true, 85.0).unwrap();
        let sof = j.windows(2).position(|w| w == [0xff, 0xc2]).unwrap();
        assert_eq!(j[sof + 9], 1, "components");
    }

    #[test]
    fn refuses_what_it_cannot_encode() {
        assert_eq!(encode(&[], 0, 10, true, 85.0), Err(Error::Dimensions));
        assert_eq!(encode(&[0; 10], 4, 4, true, 85.0), Err(Error::BufferSize));
        assert_eq!(encode(&[0; 16], 4, 4, true, 0.0), Err(Error::Quality));
        assert_eq!(
            encode(&[0; 3], 1, 70_000, false, 85.0),
            Err(Error::Dimensions)
        );
    }
}
