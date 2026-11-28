//! Page variants: smaller copies of a page in JPEG, WebP and AVIF, made once by the
//! worker so that serving never resizes or transcodes anything.
//!
//! - Widths [`WIDTHS`], only those narrower than the page. The original is
//!   always served as it is, so a page no wider than 720 pixels has none.
//! - A page that gets those also gets a thumbnail, [`THUMB`] wide, for the
//!   reader's thumbnails and scrubber. Pages made before thumbnails get
//!   theirs from their 720 JPEG ([`run_job`]), not from the original again.
//! - A variant no smaller than the original is dropped: the original serves.
//! - Each width in JPEG (quality [`JPEG_QUALITY`]), WebP (quality
//!   [`WEBP_QUALITY`]) and AVIF (quality [`AVIF_QUALITY`], speed
//!   [`AVIF_SPEED`]). Pages made before AVIF get their AVIF ones on the
//!   worker's next pass, from the original ([`run_job`]).
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

/// Width of the thumbnail, made for every page that gets variants: enough
/// for a 72-pixel-high thumbnail or the scrubber's preview on a sharp
/// phone screen, a few KB where the 720 copy of a photo is a hundred.
pub const THUMB: u32 = 240;

pub const JPEG_QUALITY: u8 = 82;
pub const WEBP_QUALITY: f32 = 80.0;

/// AVIF variants through rav1e: the optimize presets' quality, at rav1e's
/// fastest speed. On a made-up line-art page 1440 wide, speed 10 took a
/// fifth of speed 6's time for a file 13% bigger, still smaller than the
/// WebP one; variants are made for every page, so time wins here. One
/// thread, as for everything else the worker makes.
pub const AVIF_QUALITY: f32 = 70.0;
pub const AVIF_SPEED: u8 = 10;

/// Largest difference between a pixel's channels for it to count as gray:
/// scans and JPEG chroma leave a little noise on black-and-white pages.
pub const GRAY_TOLERANCE: u8 = 8;

/// Changes whenever what [`make`] produces changes (widths, formats,
/// qualities): pages made under an older version are made again, with new
/// file names and URLs (see [`url`]).
pub const VERSION: i64 = 1;

/// A variant's format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    Jpeg,
    Webp,
    Avif,
}

/// Every format's bit: what a page's variants are made in now.
pub const FORMATS: u8 = 1 | 2 | 4;

impl Format {
    pub const ALL: [Format; 3] = [Format::Jpeg, Format::Webp, Format::Avif];

    /// The file extension, as in the page URL.
    pub fn ext(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Webp => "webp",
            Format::Avif => "avif",
        }
    }

    /// Its bit, as the reader's script and `variant_pages.formats` have it.
    pub fn bit(self) -> u8 {
        match self {
            Format::Jpeg => 1,
            Format::Webp => 2,
            Format::Avif => 4,
        }
    }

    /// The MIME type, as in a `<source type>`.
    pub fn mime(self) -> &'static str {
        match self {
            Format::Jpeg => "image/jpeg",
            Format::Webp => "image/webp",
            Format::Avif => "image/avif",
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

/// The widths of the variants a page of `width` pixels gets: the
/// thumbnail, then those of [`WIDTHS`] narrower than the page.
pub fn widths_for(width: u32) -> impl Iterator<Item = u32> {
    widths_in(width, ALL_WIDTHS)
}

/// [`widths_for`] with only the widths of [`WIDTHS`] whose bit
/// ([`width_bit`]) is in `mask`, as a library's setting chooses them. The
/// thumbnail is always made.
pub fn widths_in(width: u32, mask: u8) -> impl Iterator<Item = u32> {
    let thumb = (width > WIDTHS[0]).then_some(THUMB);
    thumb.into_iter().chain(
        WIDTHS
            .into_iter()
            .filter(move |&w| w < width && mask & width_bit(w) != 0),
    )
}

/// Every width of [`WIDTHS`]: what a library has until its setting is changed.
pub const ALL_WIDTHS: u8 = 1 | 2 | 4;

/// A width's bit in a library's setting and `variant_pages.widths`: 1 for
/// 720, 2 for 1080, 4 for 1440; 0 for any other (the thumbnail).
pub fn width_bit(width: u32) -> u8 {
    WIDTHS
        .iter()
        .position(|&w| w == width)
        .map_or(0, |i| 1 << i)
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
    make_some(bytes, limits, ALL_WIDTHS, true, &Format::ALL)
}

/// The variants of a page at the widths of `mask` ([`widths_in`]; the
/// thumbnail too if `thumb`), in `formats`. [`make`] makes them all; a page
/// made before AVIF gets its AVIF ones, and one whose library wants more
/// widths now gets those, this way.
pub fn make_some(
    bytes: &[u8],
    limits: &Limits,
    mask: u8,
    thumb: bool,
    formats: &[Format],
) -> Result<Vec<Variant>, Error> {
    let pixels = decode(bytes, limits)?;
    let widths: Vec<u32> = widths_in(pixels.width, mask)
        .filter(|&w| thumb || w != THUMB)
        .collect();
    resized(pixels, &widths, formats, bytes.len())
}

/// The thumbnail of a page whose variants were made before thumbnails
/// were, from `bytes`, its narrowest WebP variant: a small picture to
/// decode, where the original may be a 50-megapixel photo.
pub fn thumb_from(bytes: &[u8], limits: &Limits) -> Result<Vec<Variant>, Error> {
    let pixels = decode(bytes, limits)?;
    if pixels.width <= THUMB {
        return Ok(Vec::new());
    }
    // No size limit: without a thumbnail the reader loads a 720 copy, which
    // is bigger, and the WebP decoded here can be smaller than a JPEG
    // thumbnail.
    resized(pixels, &[THUMB], &[Format::Jpeg, Format::Webp], usize::MAX)
}

/// `pixels` at each of `widths`, in each of `formats`, but for those no
/// smaller than `original_len` bytes.
fn resized(
    pixels: Pixels,
    widths: &[u32],
    formats: &[Format],
    original_len: usize,
) -> Result<Vec<Variant>, Error> {
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
    for &width in widths {
        let height = height_for(page_width, page_height, width);
        let mut dst = Image::new(width, height, pixel_type);
        resizer
            .resize(&src, &mut dst, &options)
            .map_err(|e| Error::Encode(e.to_string()))?;
        let data = dst.into_vec();
        for &format in formats {
            let bytes = match format {
                Format::Avif => encode_avif(&data, width, height, gray)?,
                Format::Jpeg => encode_jpeg(&data, width, height, gray)?,
                Format::Webp => match encode_webp(&data, width, height, gray) {
                    Ok(b) => b,
                    // WebP stops at 16383 pixels: a very long strip has no
                    // WebP one.
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

fn encode_avif(data: &[u8], width: u32, height: u32, gray: bool) -> Result<Vec<u8>, Error> {
    // ravif takes RGB; a gray page's flat chroma costs next to nothing.
    let rgb: Vec<ravif::RGB8> = if gray {
        data.iter().map(|&v| ravif::RGB8::new(v, v, v)).collect()
    } else {
        data.chunks_exact(3)
            .map(|p| ravif::RGB8::new(p[0], p[1], p[2]))
            .collect()
    };
    let (w, h) = (width as usize, height as usize);
    ravif::Encoder::new()
        .with_quality(AVIF_QUALITY)
        .with_speed(AVIF_SPEED)
        .encode_rgb(ravif::Img::new(&rgb[..], w, h))
        .map(|e| e.avif_file)
        .map_err(|e| Error::Encode(e.to_string()))
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
        let widths = catalog.wanted_widths(&hash)?;
        let made = make_some(&bytes, &limits, widths, true, &Format::ALL);
        drop(bytes);
        match made {
            Ok(variants) => {
                let mut rows = Vec::with_capacity(variants.len());
                for v in &variants {
                    write_blob(blobs, &hash, v).map_err(|e| {
                        let path = blobs.variant_path(&hash, v.width, VERSION, v.format.ext());
                        ScanError::Io(path, e)
                    })?;
                    rows.push((v.width, v.height, v.format.ext(), v.bytes.len() as u64));
                }
                catalog.store_variants(&hash, VERSION, (FORMATS, widths), &rows, None)?;
            }
            Err(e) => {
                if !matches!(e, Error::Unsupported(_)) {
                    log::error(
                        "variants: page left as it is",
                        &[("path", &place.book_path), ("error", &e.to_string())],
                    );
                }
                let e = e.to_string();
                catalog.store_variants(&hash, VERSION, (FORMATS, widths), &[], Some(&e))?;
            }
        }
        done += 1;
    }
    let avif = Format::Avif.bit();
    for (hash, place) in catalog.pages_lacking_format(rel, VERSION, WIDTHS[0], avif)? {
        if should_stop() {
            return Ok((JobEnd::Stopped, done));
        }
        let widths = catalog.made_widths(&hash)?.unwrap_or(ALL_WIDTHS);
        let made = read_page(library, blobs, &hash, &place)
            .map_err(|e| e.to_string())
            .and_then(|b| {
                make_some(&b, &limits, widths, true, &[Format::Avif]).map_err(|e| e.to_string())
            });
        match made {
            Ok(variants) => {
                let mut rows = Vec::with_capacity(variants.len());
                for v in &variants {
                    write_blob(blobs, &hash, v).map_err(|e| {
                        let path = blobs.variant_path(&hash, v.width, VERSION, v.format.ext());
                        ScanError::Io(path, e)
                    })?;
                    rows.push((v.width, v.height, v.format.ext(), v.bytes.len() as u64));
                }
                catalog.add_variants(&hash, &rows)?;
            }
            // It keeps its JPEG and WebP ones.
            Err(e) => log::error(
                "variants: no AVIF ones",
                &[("path", &place.book_path), ("error", &e)],
            ),
        }
        catalog.add_formats(&hash, avif)?;
        done += 1;
    }
    for (hash, place, formats, made, wanted) in
        catalog.pages_with_other_widths(rel, VERSION, WIDTHS[0])?
    {
        if should_stop() {
            return Ok((JobEnd::Stopped, done));
        }
        other_widths(
            library,
            blobs,
            catalog,
            (&hash, &place),
            formats,
            made,
            wanted,
            &limits,
        )?;
        done += 1;
    }
    for hash in catalog.pages_lacking_width(rel, VERSION, WIDTHS[0], THUMB)? {
        if should_stop() {
            return Ok((JobEnd::Stopped, done));
        }
        add_thumb(blobs, catalog, &hash, &limits)?;
        done += 1;
    }
    Ok((JobEnd::Done, done))
}

/// Brings a page made for the widths `made` to those its libraries want
/// now, `wanted`: the copies at widths no longer wanted are removed, those
/// newly wanted made from the page itself, in the formats it has (`formats`).
/// A page that cannot be read or made now keeps what it has, and is taken
/// as done, to be tried again only if the setting changes again.
#[allow(clippy::too_many_arguments)]
fn other_widths(
    library: &Path,
    blobs: &BlobStore,
    catalog: &mut Catalog,
    (hash, place): (&blake3::Hash, &PageLocation),
    formats: u8,
    made: u8,
    wanted: u8,
    limits: &Limits,
) -> Result<(), ScanError> {
    let mut now = made;
    for w in WIDTHS {
        let bit = width_bit(w);
        if made & bit != 0 && wanted & bit == 0 {
            catalog.remove_width(hash, w)?;
            for f in Format::ALL {
                let path = blobs.variant_path(hash, w, VERSION, f.ext());
                match fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(ScanError::Io(path, e)),
                }
            }
            now &= !bit;
        }
    }
    let missing = wanted & !made;
    if missing != 0 {
        let formats: Vec<Format> = Format::ALL
            .into_iter()
            .filter(|f| formats & f.bit() != 0)
            .collect();
        let result = read_page(library, blobs, hash, place)
            .map_err(|e| e.to_string())
            .and_then(|b| {
                make_some(&b, limits, missing, false, &formats).map_err(|e| e.to_string())
            });
        match result {
            Ok(variants) => {
                let mut rows = Vec::with_capacity(variants.len());
                for v in &variants {
                    write_blob(blobs, hash, v).map_err(|e| {
                        ScanError::Io(
                            blobs.variant_path(hash, v.width, VERSION, v.format.ext()),
                            e,
                        )
                    })?;
                    rows.push((v.width, v.height, v.format.ext(), v.bytes.len() as u64));
                }
                catalog.add_variants(hash, &rows)?;
                now |= missing;
            }
            Err(e) => {
                log::error(
                    "variants: no copies at the widths wanted now",
                    &[("path", &place.book_path), ("error", &e)],
                );
                now = wanted;
            }
        }
    }
    catalog.set_widths(hash, now)?;
    Ok(())
}

/// Adds the thumbnail of a page made before thumbnails were, from its 720
/// WebP. A page whose WebP cannot be read or decoded stays without one (the
/// reader uses the 720 copy instead) and is tried again on the next pass.
///
/// Not from the 720 JPEG: `image`'s JPEG decoder misreads the colour JPEGs
/// `jpeg-encoder` writes (optimised Huffman tables, 4:2:0), where browsers
/// and libjpeg read them right. Schema 17 dropped the thumbnails made from
/// them; any thumbnail file found here is such a leftover, and replaced.
fn add_thumb(
    blobs: &BlobStore,
    catalog: &mut Catalog,
    hash: &blake3::Hash,
    limits: &Limits,
) -> Result<(), ScanError> {
    for f in Format::ALL {
        let path = blobs.variant_path(hash, THUMB, VERSION, f.ext());
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(ScanError::Io(path, e)),
        }
    }
    let from = blobs.variant_path(hash, WIDTHS[0], VERSION, Format::Webp.ext());
    let made = fs::read(&from)
        .map_err(|e| e.to_string())
        .and_then(|b| thumb_from(&b, limits).map_err(|e| e.to_string()));
    let variants = match made {
        Ok(v) => v,
        Err(e) => {
            log::error(
                "variants: no thumbnail",
                &[("hash", hash.to_hex().as_ref()), ("error", &e)],
            );
            Vec::new()
        }
    };
    let mut rows = Vec::with_capacity(variants.len());
    for v in &variants {
        write_blob(blobs, hash, v).map_err(|e| {
            ScanError::Io(
                blobs.variant_path(hash, v.width, VERSION, v.format.ext()),
                e,
            )
        })?;
        rows.push((v.width, v.height, v.format.ext(), v.bytes.len() as u64));
    }
    catalog.add_variants(hash, &rows)?;
    Ok(())
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
        catalog.store_variants(&hash, VERSION, (FORMATS, ALL_WIDTHS), &[], Some(e))?;
    } else if let Some((hash, place)) = catalog
        .pages_lacking_format(rel, VERSION, WIDTHS[0], Format::Avif.bit())?
        .into_iter()
        .next()
    {
        log::error(
            "variants: no AVIF ones",
            &[
                ("path", &place.book_path),
                (
                    "error",
                    "gave up: the worker stopped while making them each time",
                ),
            ],
        );
        catalog.add_formats(&hash, Format::Avif.bit())?;
    } else if let Some((hash, place, _, _, wanted)) = catalog
        .pages_with_other_widths(rel, VERSION, WIDTHS[0])?
        .into_iter()
        .next()
    {
        log::error(
            "variants: no copies at the widths wanted now",
            &[
                ("path", &place.book_path),
                (
                    "error",
                    "gave up: the worker stopped while making them each time",
                ),
            ],
        );
        catalog.set_widths(&hash, wanted)?;
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

fn write_blob(blobs: &BlobStore, hash: &blake3::Hash, v: &Variant) -> io::Result<()> {
    let (temp, mut file) = blobs.temp()?;
    file.write_all(&v.bytes)?;
    file.sync_data()?;
    drop(file);
    blobs
        .commit_variant(&temp, hash, v.width, VERSION, v.format.ext())
        .map(drop)
}

/// The URL of a page's variant: `/p/<hash>.<width>.v<version>.<format>`.
/// The version is in it so that variants made again under new settings get
/// new URLs: the old ones stay cached, for a year, as they were.
pub fn url(hash: &blake3::Hash, width: u32, format: Format) -> String {
    format!(
        "/p/{}.{}",
        hash.to_hex(),
        crate::blobs::variant_suffix(width, VERSION, format.ext())
    )
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

    /// The average red, green and blue of a picture, as Koma decodes it.
    fn mean_rgb(bytes: &[u8]) -> [f64; 3] {
        let p = decode(bytes, &Limits::default()).unwrap();
        let mut sum = [0f64; 3];
        for px in p.data.chunks_exact(3) {
            for c in 0..3 {
                sum[c] += f64::from(px[c]);
            }
        }
        let n = (p.data.len() / 3) as f64;
        sum.map(|s| s / n)
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

    /// A JPEG or WebP variant decoded; AVIF ones are not decoded.
    fn decoded(v: &Variant) -> Option<DynamicImage> {
        let format = match v.format {
            Format::Jpeg => ImageFormat::Jpeg,
            Format::Webp => ImageFormat::WebP,
            Format::Avif => return None,
        };
        Some(image::load_from_memory_with_format(&v.bytes, format).unwrap())
    }

    /// A variant's size from its header (the only way for AVIF ones).
    fn header_size(v: &Variant) -> (u32, u32) {
        let s = crate::archive::size::read_size(&v.bytes[..], 0, v.bytes.len() as u64).unwrap();
        (s.width, s.height)
    }

    #[test]
    fn widths_below_the_page_only() {
        assert_eq!(widths_for(720).count(), 0);
        assert_eq!(widths_for(721).collect::<Vec<_>>(), [THUMB, 720]);
        assert_eq!(widths_for(1440).collect::<Vec<_>>(), [THUMB, 720, 1080]);
        assert_eq!(
            widths_for(5000).collect::<Vec<_>>(),
            [THUMB, 720, 1080, 1440]
        );
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
                (THUMB, 360, Format::Jpeg),
                (THUMB, 360, Format::Webp),
                (THUMB, 360, Format::Avif),
                (720, 1080, Format::Jpeg),
                (720, 1080, Format::Webp),
                (720, 1080, Format::Avif),
                (1080, 1620, Format::Jpeg),
                (1080, 1620, Format::Webp),
                (1080, 1620, Format::Avif),
            ]
        );
        for v in &vs {
            assert_eq!(header_size(v), (v.width, v.height), "{v:?}");
            if let Some(img) = decoded(v) {
                assert_eq!((img.width(), img.height()), (v.width, v.height));
                assert!(img.color().has_color());
            }
        }
        assert_eq!(&vs[2].bytes[4..12], b"ftypavif");
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
        assert_eq!(
            (vs[3].width, vs[3].height, vs[3].format),
            (720, 1080, Format::Jpeg)
        );
        // The stored left half (black) is now the top.
        let img = decoded(&vs[3]).unwrap().into_luma8();
        assert!(img.get_pixel(360, 100).0[0] < 50);
        assert!(img.get_pixel(360, 980).0[0] > 200);
    }

    #[test]
    fn gray_pngs_and_transparency() {
        let png = koma_fixtures::png::page(900, 1300, 3);
        let vs = make(&png, &Limits::default()).unwrap();
        assert_eq!(vs.len(), 6);
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
        assert!(
            vs.iter()
                .all(|v| v.format != Format::Jpeg || v.width == THUMB),
            "{vs:?}"
        );
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
    fn very_long_strips_keep_no_webp() {
        // 721 wide, 16,500 tall at 720: past WebP's 16,383.
        let strip = koma_fixtures::png::gray(721, 16_523, |_, y| (y % 256) as u8);
        let vs = make(&strip, &Limits::default()).unwrap();
        let got: Vec<_> = vs.iter().map(|v| (v.width, v.height, v.format)).collect();
        assert_eq!(
            got,
            [
                (THUMB, 5500, Format::Jpeg),
                (THUMB, 5500, Format::Webp),
                (THUMB, 5500, Format::Avif),
                (720, 16_500, Format::Jpeg),
                (720, 16_500, Format::Avif)
            ]
        );
    }

    #[test]
    fn a_thumbnail_from_the_720_copy() {
        let page = jpeg(1200, 1800, None, |x, y| {
            [(x % 256) as u8, (y % 256) as u8, 40]
        });
        let vs = make(&page, &Limits::default()).unwrap();
        let from = vs
            .iter()
            .find(|v| v.width == 720 && v.format == Format::Webp)
            .unwrap();
        let thumbs = thumb_from(&from.bytes, &Limits::default()).unwrap();
        let got: Vec<_> = thumbs
            .iter()
            .map(|v| (v.width, v.height, v.format))
            .collect();
        assert_eq!(
            got,
            [(THUMB, 360, Format::Jpeg), (THUMB, 360, Format::Webp)]
        );
        // And it is the same picture. Only WebP is decoded here: `image`
        // misreads the colour JPEGs `jpeg-encoder` writes (see `add_thumb`).
        let page_mean = mean_rgb(&page);
        for t in [&from.bytes, &thumbs[1].bytes] {
            let m = mean_rgb(t);
            for c in 0..3 {
                assert!((m[c] - page_mean[c]).abs() < 4.0, "{m:?} != {page_mean:?}");
            }
        }
        // Nothing to make from a picture no wider than a thumbnail.
        let small = jpeg(200, 300, None, |_, _| [9; 3]);
        assert!(thumb_from(&small, &Limits::default()).unwrap().is_empty());
    }
}
