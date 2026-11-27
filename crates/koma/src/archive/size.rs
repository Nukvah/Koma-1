//! A page's width and height, read from its header without decoding it.
//!
//! The reader lays pages out before they load, and spreads and tall strips
//! are told apart by their shape, so every page's size is stored at ingest.
//! Only headers are read: a few small reads per page, never the pixels.
//!
//! Sizes are as a browser shows the image: a JPEG whose EXIF orientation
//! turns it on its side (5 to 8), or an AVIF rotated by 90 or 270 degrees,
//! has width and height swapped.
//!
//! Headers are hostile input: every length is checked, every loop is
//! bounded, and a header that makes no sense gives `None`, never a panic.

use super::ReadAt;

/// Width and height in pixels, as displayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

impl Size {
    fn new(width: u32, height: u32) -> Option<Self> {
        (width > 0 && height > 0).then_some(Self { width, height })
    }

    fn turned(self, turn: bool) -> Self {
        if turn {
            Self {
                width: self.height,
                height: self.width,
            }
        } else {
            self
        }
    }

    /// A double-page spread: wider than 1.2 times its height.
    pub fn is_spread(self) -> bool {
        u64::from(self.width) * 10 > u64::from(self.height) * 12
    }

    /// A tall strip, as in webtoons: at least three times as tall as wide.
    pub fn is_tall_strip(self) -> bool {
        u64::from(self.height) >= u64::from(self.width) * 3
    }
}

/// Most bytes of headers read for one image: JPEG metadata segments before
/// the frame header can be large (colour profiles, thumbnails), but not this.
const MAX_SCAN: u64 = 4 * 1024 * 1024;

/// Most boxes or segments looked at in one image.
const MAX_STEPS: u32 = 4096;

/// The size of the image of `len` bytes at `offset` in `src`, or `None` if
/// its header can't be read or isn't one of the page formats.
pub fn read_size(src: &(impl ReadAt + ?Sized), offset: u64, len: u64) -> Option<Size> {
    let r = Region { src, offset, len };
    let head = r.bytes(0, 32.min(len as usize))?;
    if head.starts_with(&[0xFF, 0xD8]) {
        jpeg(&r)
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        png(&r)
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Size::new(u32::from(le16(&head, 6)?), u32::from(le16(&head, 8)?))
    } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        webp(&r)
    } else if head.starts_with(b"BM") {
        bmp(&r)
    } else if head.starts_with(&[0xFF, 0x0A]) {
        jxl_codestream(&r, 2)
    } else if head.get(4..8) == Some(b"ftyp") {
        avif(&r)
    } else if head.get(4..12) == Some(b"JXL \r\n\x87\n") {
        jxl_container(&r)
    } else {
        None
    }
}

/// A window on the source: the image's bytes.
struct Region<'a, R: ReadAt + ?Sized> {
    src: &'a R,
    offset: u64,
    len: u64,
}

impl<R: ReadAt + ?Sized> Region<'_, R> {
    fn bytes(&self, at: u64, n: usize) -> Option<Vec<u8>> {
        if at.checked_add(n as u64)? > self.len || n as u64 > MAX_SCAN {
            return None;
        }
        let mut buf = vec![0; n];
        self.src
            .read_exact_at(&mut buf, self.offset.checked_add(at)?)
            .ok()?;
        Some(buf)
    }
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}
fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
fn le24(b: &[u8], at: usize) -> Option<u32> {
    let x = b.get(at..at + 3)?;
    Some(u32::from(x[0]) | u32::from(x[1]) << 8 | u32::from(x[2]) << 16)
}

fn png<R: ReadAt + ?Sized>(r: &Region<'_, R>) -> Option<Size> {
    let h = r.bytes(8, 16)?;
    if h.get(4..8) != Some(b"IHDR") {
        return None;
    }
    Size::new(be32(&h, 8)?, be32(&h, 12)?)
}

fn bmp<R: ReadAt + ?Sized>(r: &Region<'_, R>) -> Option<Size> {
    let h = r.bytes(14, 12)?;
    if le32(&h, 0)? == 12 {
        Size::new(u32::from(le16(&h, 4)?), u32::from(le16(&h, 6)?))
    } else {
        // Height is negative for images stored top-down.
        let w = le32(&h, 4)? as i32;
        let hgt = le32(&h, 8)? as i32;
        Size::new(w.checked_abs()? as u32, hgt.checked_abs()? as u32)
    }
}

fn webp<R: ReadAt + ?Sized>(r: &Region<'_, R>) -> Option<Size> {
    let need = match r.bytes(12, 4)?.as_slice() {
        b"VP8L" => 13,
        _ => 18,
    };
    let h = r.bytes(12, need)?;
    match h.get(0..4)? {
        b"VP8 " => {
            // Frame tag (3 bytes), start code, then 14-bit sizes.
            if h.get(11..14) != Some(&[0x9D, 0x01, 0x2A]) {
                return None;
            }
            Size::new(
                u32::from(le16(&h, 14)? & 0x3FFF),
                u32::from(le16(&h, 16)? & 0x3FFF),
            )
        }
        b"VP8L" => {
            if h.get(8) != Some(&0x2F) {
                return None;
            }
            let bits = le32(&h, 9)?;
            Size::new((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1)
        }
        b"VP8X" => Size::new(le24(&h, 12)? + 1, le24(&h, 15)? + 1),
        _ => None,
    }
}

fn jpeg<R: ReadAt + ?Sized>(r: &Region<'_, R>) -> Option<Size> {
    let mut at = 2u64;
    let mut turn = false;
    for _ in 0..MAX_STEPS {
        let m = r.bytes(at, 4)?;
        if m[0] != 0xFF {
            return None;
        }
        let marker = m[1];
        // Fill bytes, and markers that carry no length.
        if marker == 0xFF {
            at += 1;
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
            at += 2;
            continue;
        }
        let seg_len = u64::from(be16(&m, 2)?);
        if seg_len < 2 {
            return None;
        }
        match marker {
            // Start of frame (not DHT C4, JPG C8, DAC CC).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let f = r.bytes(at + 4, 5)?;
                let size = Size::new(u32::from(be16(&f, 3)?), u32::from(be16(&f, 1)?))?;
                return Some(size.turned(turn));
            }
            // Start of scan or end of image before any frame: broken.
            0xDA | 0xD9 => return None,
            // APP1: EXIF, for the orientation.
            0xE1 if seg_len >= 8 => {
                let body = r.bytes(at + 4, (seg_len - 2).min(64 * 1024) as usize)?;
                if body.starts_with(b"Exif\0\0") {
                    turn = matches!(exif_orientation(&body[6..]), Some(5..=8));
                }
            }
            _ => {}
        }
        at = at.checked_add(2 + seg_len)?;
    }
    None
}

/// The orientation tag (0x0112) of the first IFD of an EXIF TIFF block.
fn exif_orientation(tiff: &[u8]) -> Option<u16> {
    let little = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |at: usize| {
        if little {
            le16(tiff, at)
        } else {
            be16(tiff, at)
        }
    };
    let u32_at = |at: usize| {
        if little {
            le32(tiff, at)
        } else {
            be32(tiff, at)
        }
    };
    let ifd = u32_at(4)? as usize;
    let count = usize::from(u16_at(ifd)?);
    for i in 0..count.min(512) {
        let entry = ifd.checked_add(2 + i * 12)?;
        if u16_at(entry)? == 0x0112 {
            return u16_at(entry + 8);
        }
    }
    None
}

/// AVIF (ISO base media): the `ispe` (image spatial extents) properties
/// under `meta/iprp/ipco`; the largest is the primary image (others are
/// alpha planes, thumbnails or grid tiles). An `irot` of 90 or 270 degrees
/// turns it.
fn avif<R: ReadAt + ?Sized>(r: &Region<'_, R>) -> Option<Size> {
    let mut best: Option<Size> = None;
    let mut turn = false;
    let mut steps = 0u32;
    // (start, end, is a full box with 4 bytes of version and flags)
    let mut stack = vec![(0u64, r.len, false)];
    while let Some((start, end, full)) = stack.pop() {
        let mut at = start + if full { 4 } else { 0 };
        while at + 8 <= end {
            steps += 1;
            if steps > MAX_STEPS {
                return None;
            }
            let h = r.bytes(at, 8)?;
            let mut size = u64::from(be32(&h, 0)?);
            let kind = h.get(4..8)?;
            let mut header = 8;
            if size == 1 {
                let big = r.bytes(at + 8, 8)?;
                size = u64::from_be_bytes(big.try_into().ok()?);
                header = 16;
            } else if size == 0 {
                size = end - at;
            }
            let box_end = at.checked_add(size)?;
            if size < header || box_end > end {
                return None;
            }
            match kind {
                b"meta" => stack.push((at + header, box_end, true)),
                b"iprp" | b"ipco" => stack.push((at + header, box_end, false)),
                b"ispe" => {
                    let b = r.bytes(at + header, 12)?;
                    if let Some(s) = Size::new(be32(&b, 4)?, be32(&b, 8)?) {
                        let area = |s: Size| u64::from(s.width) * u64::from(s.height);
                        if best.is_none_or(|b| area(s) > area(b)) {
                            best = Some(s);
                        }
                    }
                }
                b"irot" => {
                    let b = r.bytes(at + header, 1)?;
                    turn = matches!(b[0] & 3, 1 | 3);
                }
                _ => {}
            }
            at = box_end;
        }
    }
    best.map(|s| s.turned(turn))
}

/// A JPEG XL container: the size header is at the start of the `jxlc` box
/// (or the first `jxlp` box, after its 4-byte index).
fn jxl_container<R: ReadAt + ?Sized>(r: &Region<'_, R>) -> Option<Size> {
    let mut at = 0u64;
    for _ in 0..MAX_STEPS {
        let h = r.bytes(at, 8)?;
        let mut size = u64::from(be32(&h, 0)?);
        let mut header = 8;
        if size == 1 {
            size = u64::from_be_bytes(r.bytes(at + 8, 8)?.try_into().ok()?);
            header = 16;
        } else if size == 0 {
            size = r.len - at;
        }
        if size < header {
            return None;
        }
        match h.get(4..8)? {
            b"jxlc" => {
                return jxl_codestream(r, at + header + 2).filter(|_| signature(r, at + header));
            }
            b"jxlp" => {
                let start = at + header + 4;
                return jxl_codestream(r, start + 2).filter(|_| signature(r, start));
            }
            _ => {}
        }
        at = at.checked_add(size)?;
    }
    None
}

fn signature<R: ReadAt + ?Sized>(r: &Region<'_, R>, at: u64) -> bool {
    r.bytes(at, 2).is_some_and(|b| b == [0xFF, 0x0A])
}

/// A JPEG XL size header, starting `at` (just after the FF 0A signature).
fn jxl_codestream<R: ReadAt + ?Sized>(r: &Region<'_, R>, at: u64) -> Option<Size> {
    let avail = (r.len.checked_sub(at)?).min(16) as usize;
    let b = r.bytes(at, avail)?;
    let mut bits = Bits { b: &b, pos: 0 };
    let small = bits.take(1)? == 1;
    let height = if small {
        (bits.take(5)? + 1) * 8
    } else {
        bits.u32_sized()?
    };
    let ratio = bits.take(3)?;
    let width = match ratio {
        0 if small => (bits.take(5)? + 1) * 8,
        0 => bits.u32_sized()?,
        1 => height,
        2 => mul_div(height, 12, 10)?,
        3 => mul_div(height, 4, 3)?,
        4 => mul_div(height, 3, 2)?,
        5 => mul_div(height, 16, 9)?,
        6 => mul_div(height, 5, 4)?,
        _ => height.checked_mul(2)?,
    };
    Size::new(width, height)
}

fn mul_div(x: u32, num: u64, den: u64) -> Option<u32> {
    u32::try_from(u64::from(x) * num / den).ok()
}

/// Bits read least significant first, as JPEG XL stores them.
struct Bits<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn take(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for i in 0..n {
            let byte = *self.b.get(self.pos / 8)?;
            let bit = (byte >> (self.pos % 8)) & 1;
            v |= u32::from(bit) << i;
            self.pos += 1;
        }
        Some(v)
    }

    /// A dimension: 2 selector bits, then 9, 13, 18 or 30 bits, plus one.
    fn u32_sized(&mut self) -> Option<u32> {
        let n = [9, 13, 18, 30][self.take(2)? as usize];
        Some(self.take(n)? + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size(bytes: &[u8]) -> Option<(u32, u32)> {
        read_size(bytes, 0, bytes.len() as u64).map(|s| (s.width, s.height))
    }

    #[test]
    fn png_gif_bmp() {
        let png = koma_png(640, 960);
        assert_eq!(size(&png), Some((640, 960)));
        let mut gif = b"GIF89a".to_vec();
        gif.extend_from_slice(&[0x20, 0x03, 0xB0, 0x04, 0, 0, 0]);
        assert_eq!(size(&gif), Some((800, 1200)));
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0; 12]);
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&300i32.to_le_bytes());
        bmp.extend_from_slice(&(-200i32).to_le_bytes());
        assert_eq!(size(&bmp), Some((300, 200)));
    }

    /// The first bytes of a PNG with this size (enough for its header).
    fn koma_png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&[8, 2, 0, 0, 0]);
        b
    }

    fn jpeg_with(segments: &[(u8, Vec<u8>)], w: u16, h: u16) -> Vec<u8> {
        let mut b = vec![0xFF, 0xD8];
        for (marker, body) in segments {
            b.extend_from_slice(&[0xFF, *marker]);
            b.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
            b.extend_from_slice(body);
        }
        b.extend_from_slice(&[0xFF, 0xC2, 0, 11, 8]);
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&[1, 1, 0x11, 0]);
        b
    }

    fn exif(orientation: u16, little: bool) -> Vec<u8> {
        let mut t = b"Exif\0\0".to_vec();
        let w16 = |v: u16| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        let w32 = |v: u32| {
            if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            }
        };
        t.extend_from_slice(if little { b"II" } else { b"MM" });
        t.extend_from_slice(&w16(42));
        t.extend_from_slice(&w32(8));
        t.extend_from_slice(&w16(1));
        t.extend_from_slice(&w16(0x0112));
        t.extend_from_slice(&w16(3));
        t.extend_from_slice(&w32(1));
        t.extend_from_slice(&w16(orientation));
        t.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        t
    }

    #[test]
    fn jpeg_and_its_orientation() {
        let icc = vec![0u8; 60_000];
        assert_eq!(
            size(&jpeg_with(&[(0xE2, icc)], 1200, 800)),
            Some((1200, 800))
        );
        // A phone photo stored on its side: shown upright.
        for little in [true, false] {
            let turned = jpeg_with(&[(0xE1, exif(6, little))], 1200, 800);
            assert_eq!(size(&turned), Some((800, 1200)));
            let mirrored = jpeg_with(&[(0xE1, exif(2, little))], 1200, 800);
            assert_eq!(size(&mirrored), Some((1200, 800)));
        }
        assert_eq!(size(&[0xFF, 0xD8, 0xFF, 0xDA, 0, 2]), None);
    }

    #[test]
    fn webp_all_three_kinds() {
        let riff = |chunk: &[u8]| {
            let mut b = b"RIFF\0\0\0\0WEBP".to_vec();
            b.extend_from_slice(chunk);
            b
        };
        let mut lossy = b"VP8 \0\0\0\0\0\0\0\x9d\x01\x2a".to_vec();
        lossy.extend_from_slice(&700u16.to_le_bytes());
        lossy.extend_from_slice(&1000u16.to_le_bytes());
        assert_eq!(size(&riff(&lossy)), Some((700, 1000)));
        let bits: u32 = (699) | (999 << 14);
        let mut lossless = b"VP8L\0\0\0\0\x2f".to_vec();
        lossless.extend_from_slice(&bits.to_le_bytes());
        lossless.push(0);
        assert_eq!(size(&riff(&lossless)), Some((700, 1000)));
        let mut ext = b"VP8X\0\0\0\0\0\0\0\0".to_vec();
        ext.extend_from_slice(&699u32.to_le_bytes()[..3]);
        ext.extend_from_slice(&999u32.to_le_bytes()[..3]);
        assert_eq!(size(&riff(&ext)), Some((700, 1000)));
    }

    fn bx(kind: &[u8], body: &[u8]) -> Vec<u8> {
        let mut b = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn avif_largest_ispe_and_rotation() {
        let ispe = |w: u32, h: u32| {
            let mut b = vec![0, 0, 0, 0];
            b.extend_from_slice(&w.to_be_bytes());
            b.extend_from_slice(&h.to_be_bytes());
            bx(b"ispe", &b)
        };
        let build = |extra: &[u8]| {
            let mut ipco = ispe(64, 96);
            ipco.extend(ispe(1280, 1920));
            ipco.extend_from_slice(extra);
            let mut meta = vec![0, 0, 0, 0];
            meta.extend(bx(b"hdlr", &[0; 24]));
            meta.extend(bx(b"iprp", &bx(b"ipco", &ipco)));
            let mut file = bx(b"ftyp", b"avifmif1");
            file.extend(bx(b"meta", &meta));
            file
        };
        assert_eq!(size(&build(&[])), Some((1280, 1920)));
        assert_eq!(size(&build(&bx(b"irot", &[1]))), Some((1920, 1280)));
        assert_eq!(size(&build(&bx(b"irot", &[2]))), Some((1280, 1920)));
    }

    #[test]
    fn jpeg_xl_codestream_and_container() {
        // Fields written least significant bit first, after the signature.
        let codestream = |fields: &[(u64, u32)]| {
            let mut v: u64 = 0;
            let mut pos = 0;
            for &(value, n) in fields {
                v |= value << pos;
                pos += n;
            }
            let mut cs = vec![0xFF, 0x0A];
            cs.extend_from_slice(&v.to_le_bytes());
            cs
        };
        // Small: height (7+1)*8 = 64, ratio 0, width (3+1)*8 = 32.
        let small = codestream(&[(1, 1), (7, 5), (0, 3), (3, 5)]);
        assert_eq!(size(&small), Some((32, 64)));
        // Height 1000 (selector 1: 13 bits) and ratio 5 (16:9).
        let wide = codestream(&[(0, 1), (1, 2), (999, 13), (5, 3)]);
        assert_eq!(size(&wide), Some((1777, 1000)));
        // Height 1000, ratio 1 (square).
        let cs = codestream(&[(0, 1), (1, 2), (999, 13), (1, 3)]);
        assert_eq!(size(&cs), Some((1000, 1000)));
        let mut file = bx(b"JXL ", b"\r\n\x87\n");
        file.extend(bx(b"ftyp", b"jxl \0\0\0\0jxl "));
        file.extend(bx(b"jxlc", &cs));
        assert_eq!(size(&file), Some((1000, 1000)));
    }

    #[test]
    fn shapes() {
        let s = |w, h| Size {
            width: w,
            height: h,
        };
        assert!(s(1300, 1000).is_spread());
        assert!(!s(1200, 1000).is_spread());
        assert!(s(800, 2400).is_tall_strip());
        assert!(!s(800, 2399).is_tall_strip());
    }

    #[test]
    fn garbage_and_truncation_give_none() {
        assert_eq!(size(b""), None);
        assert_eq!(size(b"not an image at all"), None);
        let png = koma_png(640, 960);
        for n in 0..24 {
            assert_eq!(size(&png[..n]), None);
        }
        let jpeg = jpeg_with(&[(0xE1, exif(6, true))], 1200, 800);
        for n in 0..jpeg.len() - 4 {
            assert_eq!(size(&jpeg[..n]), None);
        }
    }
}
