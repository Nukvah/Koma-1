//! Lossy WebP encoding through libwebp, behind one safe function.
//!
//! This is the only crate of the workspace that uses `unsafe`: Rust has no
//! lossy WebP encoder, so Koma-1 calls libwebp's simple encoding API. The
//! unsafe code is the two calls below; the checks before them are what make
//! them sound.

use std::fmt;

/// libwebp's largest width or height (`WEBP_MAX_DIMENSION`).
pub const MAX_DIMENSION: u32 = 16383;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Width or height is 0 or over [`MAX_DIMENSION`].
    Dimensions,
    /// The buffer is not `width * height * 3` bytes.
    BufferSize,
    /// The quality is not between 0 and 100.
    Quality,
    /// libwebp could not encode the picture (out of memory).
    Encode,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Dimensions => "picture too large or empty for WebP",
            Error::BufferSize => "pixel buffer does not match the picture size",
            Error::Quality => "WebP quality out of range",
            Error::Encode => "libwebp failed to encode the picture",
        })
    }
}

impl std::error::Error for Error {}

/// Encodes packed 8-bit RGB pixels (`width * height * 3` bytes, rows top to
/// bottom, no padding) as a lossy WebP at `quality` (0 to 100).
pub fn encode_rgb(rgb: &[u8], width: u32, height: u32, quality: f32) -> Result<Vec<u8>, Error> {
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(Error::Dimensions);
    }
    // At most 16383 * 3 by 16383: these fit an i32 and a usize.
    let stride = width * 3;
    if rgb.len() != stride as usize * height as usize {
        return Err(Error::BufferSize);
    }
    if !(0.0..=100.0).contains(&quality) {
        return Err(Error::Quality);
    }
    let mut out: *mut u8 = std::ptr::null_mut();
    // SAFETY: `rgb` holds exactly `height` rows of `stride` bytes, checked
    // above, and libwebp only reads it. Width, height and stride are within
    // libwebp's limits and fit a c_int. `out` is written by libwebp.
    #[allow(unsafe_code)]
    let len = unsafe {
        libwebp_sys::WebPEncodeRGB(
            rgb.as_ptr(),
            width as i32,
            height as i32,
            stride as i32,
            quality,
            &mut out,
        )
    };
    let bytes = if out.is_null() || len == 0 {
        None
    } else {
        // SAFETY: on success libwebp returns a buffer of `len` bytes at `out`
        // that it allocated; it is copied before being freed below.
        #[allow(unsafe_code)]
        Some(unsafe { std::slice::from_raw_parts(out, len) }.to_vec())
    };
    if !out.is_null() {
        // SAFETY: `out` was allocated by libwebp and is freed once, here.
        #[allow(unsafe_code)]
        unsafe {
            libwebp_sys::WebPFree(out.cast());
        }
    }
    bytes.ok_or(Error::Encode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut px = Vec::with_capacity((w * h * 3) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[(x * 255 / w) as u8, (y * 255 / h) as u8, 128]);
            }
        }
        px
    }

    #[test]
    fn encodes_a_webp() {
        let webp = encode_rgb(&gradient(64, 48), 64, 48, 80.0).unwrap();
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
        // Lossy: a VP8 chunk.
        assert_eq!(&webp[12..16], b"VP8 ");
        // The frame header carries the size (14 bits each).
        let w = u16::from_le_bytes([webp[26], webp[27]]) & 0x3fff;
        let h = u16::from_le_bytes([webp[28], webp[29]]) & 0x3fff;
        assert_eq!((w, h), (64, 48));
    }

    #[test]
    fn one_pixel() {
        assert!(encode_rgb(&[0, 0, 0], 1, 1, 80.0).is_ok());
    }

    #[test]
    fn refuses_what_libwebp_would_misread() {
        assert_eq!(encode_rgb(&[], 0, 1, 80.0), Err(Error::Dimensions));
        assert_eq!(
            encode_rgb(&[0; 3], MAX_DIMENSION + 1, 1, 80.0),
            Err(Error::Dimensions)
        );
        assert_eq!(encode_rgb(&[0; 5], 1, 2, 80.0), Err(Error::BufferSize));
        assert_eq!(encode_rgb(&[0; 7], 1, 2, 80.0), Err(Error::BufferSize));
        assert_eq!(encode_rgb(&[0; 3], 1, 1, 101.0), Err(Error::Quality));
        assert_eq!(encode_rgb(&[0; 3], 1, 1, f32::NAN), Err(Error::Quality));
    }
}
