//! Page variants: smaller copies of a page in JPEG and WebP, made once by the
//! worker so that serving never resizes or transcodes anything.
//!
//! - Widths [`WIDTHS`], only those narrower than the page. The original is
//!   always served as it is, so a page no wider than 720 pixels has none.
//! - A variant no smaller than the original is dropped: the original serves.
//! - Each width in JPEG (quality [`JPEG_QUALITY`]) and WebP (quality
//!   [`WEBP_QUALITY`]). AVIF comes later (M9).
//! - Sizes are as displayed: a JPEG's EXIF orientation is applied first, so
//!   variants need none.
//! - A page whose pixels are all neutral gray, within [`GRAY_TOLERANCE`], is
//!   encoded as 8-bit gray. A tinted page (sepia, yellowed paper) keeps its
//!   colour.
//! - Transparency is flattened onto white, like paper.
//!
//! Pages are hostile input: the size is checked against [`Limits`] before
//! the pixels are decoded, and the decoder's allocations are capped too.
//! JPEG, PNG, WebP, GIF (first frame) and BMP pages are decoded; others
//! (AVIF, JPEG XL) get no variants for now and are served as they are.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Cursor, Write};
use std::os::unix::fs::FileExt;
use std::path::{Component, Path};

use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

use crate::blobs::BlobStore;
use crate::catalog::{self, Catalog, PageLocation};
use crate::log;
use crate::scan::ScanError;

/// Widths of the variants; a page gets those narrower than itself.
pub const WIDTHS: [u32; 3] = [720, 1080, 1440];

pub const JPEG_QUALITY: u8 = 82;
pub const WEBP_QUALITY: f32 = 80.0;

/// Largest difference between a pixel's channels for it to count as gray:
/// scans and JPEG chroma leave a little noise on black-and-white pages.
pub const GRAY_TOLERANCE: u8 = 8;

/// Changes whenever what [`make`] produces changes (widths, formats,
/// qualities): pages made under an older version are made again.
pub const VERSION: i64 = 1;

/// A variant's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    Jpeg,
    Webp,
}

impl Format {
    pub const ALL: [Format; 2] = [Format::Jpeg, Format::Webp];

    /// The file extension, as in the page URL.
    pub fn ext(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Webp => "webp",
        }
    }

    pub fn from_ext(ext: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.ext() == ext)
    }
}

/// One encoded variant.
#[derive(Clone)]
pub struct Variant {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    pub bytes: Vec<u8>,
}

impl fmt::Debug for Variant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}x{} {} ({} bytes)",
            self.width,
            self.height,
            self.format.ext(),
            self.bytes.len()
        )
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Most pixels of a page that is decoded (width times height).
    pub max_pixels: u64,
    /// Most bytes the decoder may allocate.
    pub max_alloc: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // A double-page spread at 600 dpi is about 50 million; a long
            // webtoon strip 800 x 60,000.
            max_pixels: 64_000_000,
            max_alloc: 512 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// A format whose pixels Koma-1 does not decode (yet): no variants.
    Unsupported(&'static str),
    /// Bigger than [`Limits`] allows.
    Limit(&'static str),
    /// The page could not be decoded.
    Decode(String),
    /// A variant could not be encoded.
    Encode(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported(what) => write!(f, "no variants for {what} pages"),
            Error::Limit(why) => write!(f, "page over limit: {why}"),
            Error::Decode(e) => write!(f, "cannot decode page: {e}"),
            Error::Encode(e) => write!(f, "cannot encode variant: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// The widths of the variants a page of `width` pixels gets.
pub fn widths_for(width: u32) -> impl Iterator<Item = u32> {
    WIDTHS.into_iter().filter(move |&w| w < width)
}

/// The height of a variant `width` pixels wide of a page `page_width` by
/// `page_height` (rounded, at least 1).
pub fn height_for(page_width: u32, page_height: u32, width: u32) -> u32 {
    let h = (u64::from(page_height) * u64::from(width) * 2 + u64::from(page_width))
        / (u64::from(page_width) * 2);
    u32::try_from(h.max(1)).unwrap_or(u32::MAX)
}

/// Decoded pixels, oriented as displayed and without alpha.
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    /// 1 byte per pixel when gray, 3 (RGB) otherwise.
    pub gray: bool,
    pub data: Vec<u8>,
}

/// Decodes a page's bytes, applying its EXIF orientation, flattening
/// transparency onto white and turning neutral-gray pages into 8-bit gray.
pub fn decode(bytes: &[u8], limits: &Limits) -> Result<Pixels, Error> {
    let format = match image::guess_format(bytes) {
        Ok(f @ (ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP)) => f,
        Ok(f @ (ImageFormat::Gif | ImageFormat::Bmp)) => f,
        Ok(ImageFormat::Avif) => return Err(Error::Unsupported("AVIF")),
        Ok(_) | Err(_) => {
            // JPEG XL has no magic number `image` knows.
            if bytes.starts_with(&[0xff, 0x0a]) || bytes.starts_with(b"\0\0\0\x0cJXL ") {
                return Err(Error::Unsupported("JPEG XL"));
            }
            return Err(Error::Unsupported("this kind of"));
        }
    };
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut image_limits = image::Limits::default();
    image_limits.max_alloc = Some(limits.max_alloc);
    reader.limits(image_limits);
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 {
        return Err(Error::Decode("empty image".into()));
    }
    if u64::from(w) * u64::from(h) > limits.max_pixels {
        return Err(Error::Limit("too many pixels"));
    }
    // Not every decoder enforces `max_alloc`: check what it will need.
    if decoder.total_bytes() > limits.max_alloc {
        return Err(Error::Limit("decoder memory"));
    }
    let orientation = decoder.orientation().map_err(decode_error)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    image.apply_orientation(orientation);
    Ok(flatten(image))
}

fn decode_error(e: image::ImageError) -> Error {
    match e {
        image::ImageError::Limits(_) => Error::Limit("decoder memory"),
        e => Error::Decode(e.to_string()),
    }
}

/// Removes alpha (onto white) and picks gray or RGB.
fn flatten(image: DynamicImage) -> Pixels {
    let (width, height) = (image.width(), image.height());
    let gray_source = !image.color().has_color();
    let data = if gray_source {
        if image.color().has_alpha() {
            let la = image.into_luma_alpha8();
            la.pixels().map(|p| over_white(p.0[0], p.0[1])).collect()
        } else {
            image.into_luma8().into_raw()
        }
    } else if image.color().has_alpha() {
        let rgba = image.into_rgba8();
        let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
        for p in rgba.pixels() {
            let [r, g, b, a] = p.0;
            rgb.extend_from_slice(&[over_white(r, a), over_white(g, a), over_white(b, a)]);
        }
        rgb
    } else {
        image.into_rgb8().into_raw()
    };
    if gray_source {
        return Pixels {
            width,
            height,
            gray: true,
            data,
        };
    }
    if is_neutral(&data) {
        let gray = data
            .chunks_exact(3)
            .map(|p| luma(p[0], p[1], p[2]))
            .collect();
        Pixels {
            width,
            height,
            gray: true,
            data: gray,
        }
    } else {
        Pixels {
            width,
            height,
            gray: false,
            data,
        }
    }
}

fn over_white(v: u8, alpha: u8) -> u8 {
    let (v, a) = (u32::from(v), u32::from(alpha));
    ((v * a + 255 * (255 - a) + 127) / 255) as u8
}

/// Every pixel's channels within [`GRAY_TOLERANCE`] of each other.
fn is_neutral(rgb: &[u8]) -> bool {
    rgb.chunks_exact(3).all(|p| {
        let max = p[0].max(p[1]).max(p[2]);
        let min = p[0].min(p[1]).min(p[2]);
        max - min <= GRAY_TOLERANCE
    })
}

/// BT.601 luma, as JPEG's Y.
fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((77 * u32::from(r) + 150 * u32::from(g) + 29 * u32::from(b) + 128) >> 8) as u8
}

/// Every variant of a page: each width in [`widths_for`], in each format,
/// except those that come out no smaller than the page itself. Empty for a
/// page no wider than the smallest width.
pub fn make(bytes: &[u8], limits: &Limits) -> Result<Vec<Variant>, Error> {
    let original_len = bytes.len();
    let pixels = decode(bytes, limits)?;
    let mut out = Vec::new();
    let mut resizer = Resizer::new();
    let options = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    let pixel_type = if pixels.gray {
        PixelType::U8
    } else {
        PixelType::U8x3
    };
    let (page_width, page_height, gray) = (pixels.width, pixels.height, pixels.gray);
    let src = Image::from_vec_u8(page_width, page_height, pixels.data, pixel_type)
        .map_err(|e| Error::Encode(e.to_string()))?;
    for width in widths_for(page_width) {
        let height = height_for(page_width, page_height, width);
        let mut dst = Image::new(width, height, pixel_type);
        resizer
            .resize(&src, &mut dst, &options)
            .map_err(|e| Error::Encode(e.to_string()))?;
        let data = dst.into_vec();
        for format in Format::ALL {
            let bytes = match format {
                Format::Jpeg => encode_jpeg(&data, width, height, gray)?,
                Format::Webp => match encode_webp(&data, width, height, gray) {
                    Ok(b) => b,
                    // WebP stops at 16383 pixels: a very long strip has only
                    // the JPEG.
                    Err(koma_webp::Error::Dimensions) => continue,
                    Err(e) => return Err(Error::Encode(e.to_string())),
                },
            };
            // A copy no smaller than the original is no use: clean line art
            // in PNG can be smaller than any lossy version of it.
            if bytes.len() >= original_len {
                continue;
            }
            out.push(Variant {
                width,
                height,
                format,
                bytes,
            });
        }
    }
    Ok(out)
}

fn encode_jpeg(data: &[u8], width: u32, height: u32, gray: bool) -> Result<Vec<u8>, Error> {
    let (Ok(w), Ok(h)) = (u16::try_from(width), u16::try_from(height)) else {
        return Err(Error::Limit("too tall for JPEG"));
    };
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, JPEG_QUALITY);
    encoder.set_optimized_huffman_tables(true);
    let color = if gray {
        jpeg_encoder::ColorType::Luma
    } else {
        jpeg_encoder::ColorType::Rgb
    };
    encoder
        .encode(data, w, h, color)
        .map_err(|e| Error::Encode(e.to_string()))?;
    Ok(out)
}

fn encode_webp(
    data: &[u8],
    width: u32,
    height: u32,
    gray: bool,
) -> Result<Vec<u8>, koma_webp::Error> {
    if gray {
        // WebP has no gray mode; flat chroma costs next to nothing.
        let rgb: Vec<u8> = data.iter().flat_map(|&v| [v, v, v]).collect();
        koma_webp::encode_rgb(&rgb, width, height, WEBP_QUALITY)
    } else {
        koma_webp::encode_rgb(data, width, height, WEBP_QUALITY)
    }
}

/// How a variants job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobEnd {
    Done,
    /// Stopped between pages: the job goes back in the queue.
    Stopped,
}

/// Makes the variants of every page of the book at `rel` that lacks them,
/// one page at a time. Each page's variants are written and recorded before
/// the next is started, so a worker killed mid-book only redoes that page.
/// Returns the job's end and how many pages were done.
pub fn run_job(
    library: &Path,
    blobs: &BlobStore,
    catalog: &mut Catalog,
    rel: &str,
    should_stop: &dyn Fn() -> bool,
) -> Result<(JobEnd, usize), ScanError> {
    let limits = Limits::default();
    let mut done = 0;
    for (hash, place) in catalog.pages_lacking_variants(rel, VERSION, WIDTHS[0])? {
        if should_stop() {
            return Ok((JobEnd::Stopped, done));
        }
        let bytes = match read_page(library, blobs, &hash, &place) {
            Ok(b) => b,
            Err(e) => {
                // The book changed or vanished since it was indexed: the
                // next scan reads it again, and its new pages get variants.
                log::info(
                    "variants: cannot read a page",
                    &[("path", &place.book_path), ("error", &e.to_string())],
                );
                continue;
            }
        };
        let made = make(&bytes, &limits);
        drop(bytes);
        match made {
            Ok(variants) => {
                let mut rows = Vec::with_capacity(variants.len());
                for v in &variants {
                    let path = blobs.variant_path(&hash, v.width, v.format.ext());
                    write_blob(blobs, &path, &v.bytes).map_err(|e| ScanError::Io(path, e))?;
                    rows.push((v.width, v.height, v.format.ext(), v.bytes.len() as u64));
                }
                catalog.store_variants(&hash, VERSION, &rows, None)?;
            }
            Err(e) => {
                if !matches!(e, Error::Unsupported(_)) {
                    log::error(
                        "variants: page left as it is",
                        &[("path", &place.book_path), ("error", &e.to_string())],
                    );
                }
                catalog.store_variants(&hash, VERSION, &[], Some(&e.to_string()))?;
            }
        }
        done += 1;
    }
    Ok((JobEnd::Done, done))
}

/// After the worker died on a book's variants job each time: the page it
/// was on (the first one still lacking variants) is given up, so the rest
/// of the book can go on.
pub fn give_up_page(catalog: &mut Catalog, rel: &str) -> Result<(), catalog::Error> {
    if let Some((hash, place)) = catalog
        .pages_lacking_variants(rel, VERSION, WIDTHS[0])?
        .into_iter()
        .next()
    {
        let e = "gave up: the worker stopped while making its variants each time";
        log::error(
            "variants: page left as it is",
            &[("path", &place.book_path), ("error", e)],
        );
        catalog.store_variants(&hash, VERSION, &[], Some(e))?;
    }
    Ok(())
}

fn read_page(
    library: &Path,
    blobs: &BlobStore,
    hash: &blake3::Hash,
    place: &PageLocation,
) -> io::Result<Vec<u8>> {
    let mut bytes = match place.offset {
        Some(offset) => {
            let rel = Path::new(&place.book_path);
            if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
                return Err(io::Error::other("unsafe path in catalogue"));
            }
            let file = File::open(library.join(rel))?;
            let len = usize::try_from(place.length).map_err(io::Error::other)?;
            let mut buf = vec![0; len];
            file.read_exact_at(&mut buf, offset)?;
            buf
        }
        None => fs::read(blobs.path(hash, &place.format))?,
    };
    // The file may have been rewritten since it was indexed.
    if blake3::hash(&bytes) != *hash {
        bytes.clear();
        return Err(io::Error::other("page changed since it was indexed"));
    }
    Ok(bytes)
}

fn write_blob(blobs: &BlobStore, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let (temp, mut file) = blobs.temp()?;
    file.write_all(bytes)?;
    file.sync_data()?;
    drop(file);
    blobs.commit_replacing(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A JPEG `w` x `h` of `pixel(x, y)` RGB, with an EXIF orientation if given.
    fn jpeg(
        w: u16,
        h: u16,
        orientation: Option<u16>,
        pixel: impl Fn(u32, u32) -> [u8; 3],
    ) -> Vec<u8> {
        jpeg_q(95, w, h, orientation, pixel)
    }

    fn jpeg_q(
        quality: u8,
        w: u16,
        h: u16,
        orientation: Option<u16>,
        pixel: impl Fn(u32, u32) -> [u8; 3],
    ) -> Vec<u8> {
        let mut rgb = Vec::new();
        for y in 0..u32::from(h) {
            for x in 0..u32::from(w) {
                rgb.extend_from_slice(&pixel(x, y));
            }
        }
        let mut out = Vec::new();
        let mut e = jpeg_encoder::Encoder::new(&mut out, quality);
        if let Some(o) = orientation {
            // Big-endian TIFF, one IFD entry: orientation (0x0112), SHORT.
            let mut t = b"MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01".to_vec();
            t.extend_from_slice(&o.to_be_bytes());
            t.extend_from_slice(&[0; 6]);
            e.add_exif_metadata(&t).unwrap();
        }
        e.encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb).unwrap();
        out
    }

    fn components_of_jpeg(j: &[u8]) -> u8 {
        // The first SOF marker: FF C0 / C1 / C2, then length (2), precision
        // (1), height (2), width (2), components (1).
        let at = j
            .windows(2)
            .position(|m| m[0] == 0xff && matches!(m[1], 0xc0..=0xc2))
            .unwrap();
        j[at + 9]
    }

    fn first_jpeg(vs: &[Variant]) -> &[u8] {
        &vs.iter()
            .find(|v| v.format == Format::Jpeg)
            .expect("a JPEG variant")
            .bytes
    }

    fn decoded(v: &Variant) -> DynamicImage {
        let format = match v.format {
            Format::Jpeg => ImageFormat::Jpeg,
            Format::Webp => ImageFormat::WebP,
        };
        image::load_from_memory_with_format(&v.bytes, format).unwrap()
    }

    #[test]
    fn widths_below_the_page_only() {
        assert_eq!(widths_for(720).count(), 0);
        assert_eq!(widths_for(721).collect::<Vec<_>>(), [720]);
        assert_eq!(widths_for(1440).collect::<Vec<_>>(), [720, 1080]);
        assert_eq!(widths_for(5000).collect::<Vec<_>>(), [720, 1080, 1440]);
    }

    #[test]
    fn heights_keep_the_shape() {
        assert_eq!(height_for(1600, 2400, 720), 1080);
        assert_eq!(height_for(3, 2, 2), 1); // 1.33 rounds down
        assert_eq!(height_for(3, 5, 2), 3); // 3.33
        assert_eq!(height_for(100_000, 1, 720), 1); // never 0
    }

    #[test]
    fn a_colour_page_in_every_width_and_format() {
        let page = jpeg(1200, 1800, None, |x, y| {
            [(x % 256) as u8, (y % 256) as u8, 40]
        });
        let vs = make(&page, &Limits::default()).unwrap();
        let got: Vec<_> = vs.iter().map(|v| (v.width, v.height, v.format)).collect();
        assert_eq!(
            got,
            [
                (720, 1080, Format::Jpeg),
                (720, 1080, Format::Webp),
                (1080, 1620, Format::Jpeg),
                (1080, 1620, Format::Webp),
            ]
        );
        for v in &vs {
            let img = decoded(v);
            assert_eq!((img.width(), img.height()), (v.width, v.height));
            assert!(img.color().has_color());
        }
        assert_eq!(components_of_jpeg(first_jpeg(&vs)), 3);
    }

    #[test]
    fn a_neutral_page_is_stored_gray_and_a_tinted_one_is_not() {
        // Black ink on white with a little scanner noise between channels.
        let ink = jpeg(1600, 2400, None, |x, y| {
            let v = if (x / 40 + y / 40) % 2 == 0 { 20 } else { 235 };
            [v, v + 3, v.saturating_sub(2)]
        });
        let vs = make(&ink, &Limits::default()).unwrap();
        assert_eq!(components_of_jpeg(first_jpeg(&vs)), 1);
        assert!(
            !image::load_from_memory(first_jpeg(&vs))
                .unwrap()
                .color()
                .has_color()
        );

        let sepia = jpeg(1600, 2400, None, |x, _| {
            let v = (x % 200) as u8 + 40;
            [v, v.saturating_sub(20), v.saturating_sub(45)]
        });
        let vs = make(&sepia, &Limits::default()).unwrap();
        assert_eq!(components_of_jpeg(first_jpeg(&vs)), 3);
    }

    #[test]
    fn exif_orientation_is_applied() {
        // Stored landscape, shown portrait: 6 turns it a quarter clockwise.
        let page = jpeg(
            1500,
            1000,
            Some(6),
            |x, _| if x < 750 { [0; 3] } else { [255; 3] },
        );
        let vs = make(&page, &Limits::default()).unwrap();
        assert_eq!((vs[0].width, vs[0].height), (720, 1080));
        // The stored left half (black) is now the top.
        let img = decoded(&vs[0]).into_luma8();
        assert!(img.get_pixel(360, 100).0[0] < 50);
        assert!(img.get_pixel(360, 980).0[0] > 200);
    }

    #[test]
    fn gray_pngs_and_transparency() {
        let png = koma_fixtures::png::page(900, 1300, 3);
        let vs = make(&png, &Limits::default()).unwrap();
        assert_eq!(vs.len(), 2);
        assert_eq!(components_of_jpeg(first_jpeg(&vs)), 1);

        // Fully transparent becomes white.
        assert_eq!(over_white(0, 0), 255);
        assert_eq!(over_white(0, 255), 0);
        assert_eq!(over_white(100, 128), 177);
    }

    #[test]
    fn a_variant_bigger_than_the_page_is_dropped() {
        // A page saved at a low quality: a 720 JPEG of it at quality 82
        // takes more bytes than the whole page.
        let texture = |x: u32, y: u32| {
            let v = ((x * 7 + y * 13) ^ (x * y)) as u8;
            [v, v / 2 + 60, 255 - v]
        };
        let page = jpeg_q(20, 1000, 1500, None, texture);
        let vs = make(&page, &Limits::default()).unwrap();
        assert!(vs.iter().all(|v| v.bytes.len() < page.len()), "{vs:?}");
        assert!(vs.iter().all(|v| v.format != Format::Jpeg), "{vs:?}");
    }

    #[test]
    fn small_pages_have_none() {
        let png = koma_fixtures::png::page(60, 90, 1);
        assert!(make(&png, &Limits::default()).unwrap().is_empty());
    }

    #[test]
    fn limits_and_garbage() {
        let page = jpeg(1000, 1000, None, |_, _| [9; 3]);
        let tight = Limits {
            max_pixels: 999_999,
            ..Limits::default()
        };
        assert_eq!(
            make(&page, &tight).unwrap_err(),
            Error::Limit("too many pixels")
        );
        let tiny_alloc = Limits {
            max_alloc: 1024,
            ..Limits::default()
        };
        assert!(matches!(make(&page, &tiny_alloc), Err(Error::Limit(_))));

        assert!(matches!(
            make(b"not an image", &Limits::default()),
            Err(Error::Unsupported(_))
        ));
        // A cut JPEG decodes with its missing rows filled, as browsers show it;
        // a cut PNG does not.
        let png = koma_fixtures::png::page(900, 1300, 3);
        assert!(matches!(
            make(&png[..png.len() / 3], &Limits::default()),
            Err(Error::Decode(_))
        ));
        let mut avif = b"\0\0\0\x1cftypavif".to_vec();
        avif.resize(64, 0);
        assert_eq!(
            make(&avif, &Limits::default()).unwrap_err(),
            Error::Unsupported("AVIF")
        );
        assert_eq!(
            make(&[0xff, 0x0a, 0, 0], &Limits::default()).unwrap_err(),
            Error::Unsupported("JPEG XL")
        );
    }

    #[test]
    fn very_long_strips_keep_only_the_jpeg() {
        // 721 wide, 16,500 tall at 720: past WebP's 16,383.
        let strip = koma_fixtures::png::gray(721, 16_523, |_, y| (y % 256) as u8);
        let vs = make(&strip, &Limits::default()).unwrap();
        let got: Vec<_> = vs.iter().map(|v| (v.width, v.height, v.format)).collect();
        assert_eq!(got, [(720, 16_500, Format::Jpeg)]);
    }
}
