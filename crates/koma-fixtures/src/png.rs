//! Minimal valid PNG files: 8-bit gray or RGB, uncompressed (stored DEFLATE).

use crate::checksum::{adler32, crc32, deflate_stored};

/// A `width` × `height` page whose look depends on `seed`, so every page has
/// different bytes: a background shade plus a checker pattern.
pub fn page(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let shade = (seed.wrapping_mul(37) % 200) as u8 + 30;
    let cell = 4 + seed % 5;
    gray(width, height, |x, y| {
        if (x / cell + y / cell).is_multiple_of(2) {
            shade
        } else {
            255 - shade
        }
    })
}

pub fn gray(width: u32, height: u32, pixel: impl Fn(u32, u32) -> u8) -> Vec<u8> {
    let mut raw = Vec::with_capacity((height * (width + 1)) as usize);
    for y in 0..height {
        raw.push(0); // filter: none
        raw.extend((0..width).map(|x| pixel(x, y)));
    }
    encode(width, height, 0, &raw)
}

/// An 8-bit RGB picture of `pixel(x, y)`.
pub fn rgb(width: u32, height: u32, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let mut raw = Vec::with_capacity((height * (width * 3 + 1)) as usize);
    for y in 0..height {
        raw.push(0); // filter: none
        raw.extend((0..width).flat_map(|x| pixel(x, y)));
    }
    encode(width, height, 2, &raw)
}

/// A PNG of filtered scanlines `raw`, 8 bits a sample, of colour type
/// `colour` (0: gray, 2: RGB).
fn encode(width: u32, height: u32, colour: u8, raw: &[u8]) -> Vec<u8> {
    let mut zlib = vec![0x78, 0x01];
    zlib.extend(deflate_stored(raw));
    zlib.extend(adler32(raw).to_be_bytes());

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend(width.to_be_bytes());
    ihdr.extend(height.to_be_bytes());
    ihdr.extend([8, colour, 0, 0, 0]); // 8-bit, deflate, no filter, no interlace

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    chunk(&mut png, b"IEND", &[]);
    png
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend(kind);
    out.extend(data);
    let crc = crc32(&out[start..]);
    out.extend(crc.to_be_bytes());
}

#[cfg(test)]
mod tests {
    #[test]
    fn pages_differ_and_look_like_png() {
        let a = super::page(30, 45, 1);
        let b = super::page(30, 45, 2);
        assert_ne!(a, b);
        assert!(a.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(a.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82]));
    }

    #[test]
    fn rgb_pictures_say_so() {
        let p = super::rgb(3, 2, |x, y| [x as u8, y as u8, 7]);
        // IHDR: width 3, height 2, 8 bits, colour type 2.
        assert_eq!(&p[16..26], &[0, 0, 0, 3, 0, 0, 0, 2, 8, 2]);
        // Two scanlines of a filter byte and 3 pixels, stored.
        assert!(p.windows(10).any(|w| w == [0, 0, 0, 7, 1, 0, 7, 2, 0, 7]));
    }
}
