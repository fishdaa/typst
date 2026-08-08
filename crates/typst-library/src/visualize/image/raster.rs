use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::io;
use std::sync::{Arc, OnceLock};

use crate::diag::{StrResult, bail};
use crate::foundations::{Bytes, Cast, Dict, Smart, Value, cast, dict};
use ecow::{EcoString, eco_format};
use image::codecs::gif::GifDecoder;
use image::codecs::jpeg::JpegDecoder;
use image::codecs::png::PngDecoder;
use image::codecs::webp::WebPDecoder;
use image::{
    DynamicImage, ImageBuffer, ImageDecoder, ImageResult, Limits, Pixel, guess_format,
};

/// A decoded raster image.
#[derive(Clone, Hash)]
pub struct RasterImage(Arc<RasterImageInner>);

/// The internal representation of a [`RasterImage`].
struct RasterImageInner {
    data: Bytes,
    format: RasterFormat,
    /// The image's final pixel dimensions, after any EXIF-driven transpose.
    /// Known up front from the decoder's header without a full pixel decode.
    width: u32,
    height: u32,
    /// Whether the image has an alpha channel. Also known from the header
    /// alone (color type plus presence of a `tRNS` chunk), without decoding
    /// pixels.
    has_alpha: bool,
    /// The fully decoded image, decoded lazily on first access via
    /// [`RasterImage::dynamic`] -- so a caller that only ever needs a region
    /// of the image (see [`RasterImage::decode_rgba_row_range`]) never pays
    /// for the full decode.
    dynamic: OnceLock<Arc<DynamicImage>>,
    exif_rotation: Option<u32>,
    icc: Option<Bytes>,
    dpi: Option<f64>,
}

impl RasterImage {
    /// Decode a raster image.
    pub fn new(
        data: Bytes,
        format: impl Into<RasterFormat>,
        icc: Smart<Bytes>,
    ) -> StrResult<Self> {
        Self::new_impl(data, format.into(), icc)
    }

    /// Create a raster image with optional properties set to the default.
    pub fn plain(data: Bytes, format: impl Into<RasterFormat>) -> StrResult<Self> {
        Self::new(data, format, Smart::Auto)
    }

    /// The internal, non-generic implementation.
    #[comemo::memoize]
    #[typst_macros::time(name = "load raster image")]
    fn new_impl(
        data: Bytes,
        format: RasterFormat,
        icc: Smart<Bytes>,
    ) -> StrResult<RasterImage> {
        let mut exif_rot = None;

        // For PNG, the full pixel buffer is decoded lazily (see
        // `decode_dynamic`) -- constructing a `RasterImage` here only reads
        // header/metadata and validates that the rest of the image data
        // decodes successfully by streaming through it row by row and
        // discarding each row, which bounds memory to a single row rather
        // than the whole image while still failing fast on a malformed
        // file, exactly like the eager formats below.
        let (width, height, has_alpha, icc, dpi, eager_dynamic) = match format {
            RasterFormat::Exchange(ExchangeFormat::Png) => {
                let (raw_w, raw_h, has_alpha, icc) = validate_png(&data, icc)?;

                let exif = exif::Reader::new()
                    .read_from_container(&mut io::Cursor::new(&data))
                    .ok();

                let (mut width, mut height) = (raw_w, raw_h);
                if let Some(rotation) = exif.as_ref().and_then(exif_rotation) {
                    if matches!(rotation, 5 | 6 | 7 | 8) {
                        std::mem::swap(&mut width, &mut height);
                    }
                    exif_rot = Some(rotation);
                }

                let dpi = determine_dpi(&data, exif.as_ref());
                (width, height, has_alpha, icc, dpi, None)
            }

            RasterFormat::Exchange(format) => {
                fn decode<T: ImageDecoder>(
                    decoder: ImageResult<T>,
                    icc: Smart<Bytes>,
                ) -> ImageResult<(image::DynamicImage, Option<Bytes>)> {
                    let mut decoder = decoder?;
                    let icc = icc.custom().or_else(|| {
                        decoder
                            .icc_profile()
                            .ok()
                            .flatten()
                            .filter(|icc| !icc.is_empty())
                            .map(Bytes::new)
                    });
                    decoder.set_limits(Limits::default())?;
                    let dynamic = image::DynamicImage::from_decoder(decoder)?;
                    Ok((dynamic, icc))
                }

                let cursor = io::Cursor::new(&data);
                let (mut dynamic, icc) = match format {
                    ExchangeFormat::Jpg => decode(JpegDecoder::new(cursor), icc),
                    ExchangeFormat::Gif => decode(GifDecoder::new(cursor), icc),
                    ExchangeFormat::Webp => decode(WebPDecoder::new(cursor), icc),
                    ExchangeFormat::Png => {
                        unreachable!("handled by the branch above")
                    }
                }
                .map_err(format_image_error)?;

                let exif = exif::Reader::new()
                    .read_from_container(&mut io::Cursor::new(&data))
                    .ok();

                // Apply rotation from EXIF metadata.
                if let Some(rotation) = exif.as_ref().and_then(exif_rotation) {
                    apply_rotation(&mut dynamic, rotation);
                    exif_rot = Some(rotation);
                }

                // Extract pixel density.
                let dpi = determine_dpi(&data, exif.as_ref());
                let has_alpha = dynamic.color().has_alpha();
                let (width, height) = (dynamic.width(), dynamic.height());

                (width, height, has_alpha, icc, dpi, Some(dynamic))
            }

            RasterFormat::Pixel(format) => {
                if format.width == 0 || format.height == 0 {
                    bail!("zero-sized images are not allowed");
                }

                let channels = match format.encoding {
                    PixelEncoding::Rgb8 => 3,
                    PixelEncoding::Rgba8 => 4,
                    PixelEncoding::Luma8 => 1,
                    PixelEncoding::Lumaa8 => 2,
                };

                let Some(expected_size) = format
                    .width
                    .checked_mul(format.height)
                    .and_then(|size| size.checked_mul(channels))
                else {
                    bail!("pixel dimensions are too large");
                };

                if expected_size as usize != data.len() {
                    bail!("pixel dimensions and pixel data do not match");
                }

                fn to<P: Pixel<Subpixel = u8>>(
                    data: &Bytes,
                    format: PixelFormat,
                ) -> ImageBuffer<P, Vec<u8>> {
                    ImageBuffer::from_raw(format.width, format.height, data.to_vec())
                        .unwrap()
                }

                let dynamic: DynamicImage = match format.encoding {
                    PixelEncoding::Rgb8 => to::<image::Rgb<u8>>(&data, format).into(),
                    PixelEncoding::Rgba8 => to::<image::Rgba<u8>>(&data, format).into(),
                    PixelEncoding::Luma8 => to::<image::Luma<u8>>(&data, format).into(),
                    PixelEncoding::Lumaa8 => to::<image::LumaA<u8>>(&data, format).into(),
                };

                let has_alpha = matches!(
                    format.encoding,
                    PixelEncoding::Rgba8 | PixelEncoding::Lumaa8
                );

                (
                    format.width,
                    format.height,
                    has_alpha,
                    icc.custom(),
                    None,
                    Some(dynamic),
                )
            }
        };

        let dynamic_cell = OnceLock::new();
        if let Some(dynamic) = eager_dynamic {
            dynamic_cell.set(Arc::new(dynamic)).ok();
        }

        Ok(Self(Arc::new(RasterImageInner {
            data,
            format,
            width,
            height,
            has_alpha,
            dynamic: dynamic_cell,
            exif_rotation: exif_rot,
            icc,
            dpi,
        })))
    }

    /// Fully decodes the image, if it wasn't already -- this is deferred
    /// for PNG (see `new_impl`) so that a caller which only ever needs a
    /// region of the image (via [`Self::decode_rgba_row_range`]) never
    /// pays for a full decode.
    fn decode_dynamic(&self) -> &Arc<DynamicImage> {
        self.0.dynamic.get_or_init(|| {
            let RasterFormat::Exchange(ExchangeFormat::Png) = self.0.format else {
                unreachable!("other formats are decoded eagerly in `new_impl`")
            };

            let cursor = io::Cursor::new(&self.0.data);
            let decoder = PngDecoder::new(cursor)
                .and_then(|mut d| {
                    d.set_limits(Limits::default())?;
                    Ok(d)
                })
                .expect("already validated in `new_impl`");
            let mut dynamic = image::DynamicImage::from_decoder(decoder)
                .expect("already validated in `new_impl`");

            if let Some(rotation) = self.0.exif_rotation {
                apply_rotation(&mut dynamic, rotation);
            }

            Arc::new(dynamic)
        })
    }

    /// The raw image data.
    pub fn data(&self) -> &Bytes {
        &self.0.data
    }

    /// The image's format.
    pub fn format(&self) -> RasterFormat {
        self.0.format
    }

    /// The image's pixel width.
    pub fn width(&self) -> u32 {
        self.0.width
    }

    /// The image's pixel height.
    pub fn height(&self) -> u32 {
        self.0.height
    }

    /// Whether the image has an alpha channel.
    pub fn has_alpha(&self) -> bool {
        self.0.has_alpha
    }

    /// The EXIF orientation value of the original image.
    ///
    /// The [`dynamic`](Self::dynamic) image already has this factored in. This
    /// value is only relevant to consumers of the raw [`data`](Self::data).
    pub fn exif_rotation(&self) -> Option<u32> {
        self.0.exif_rotation
    }

    /// The image's pixel density in pixels per inch, if known.
    ///
    /// This is guaranteed to be positive.
    pub fn dpi(&self) -> Option<f64> {
        self.0.dpi
    }

    /// Access the underlying dynamic image.
    ///
    /// For a PNG image, this decodes the whole image if it hasn't been
    /// already. A caller that only needs a horizontal strip of the image
    /// (e.g. one band of a large banded render) should prefer
    /// [`Self::decode_rgba_row_range`] where possible, to avoid paying for
    /// a full decode.
    pub fn dynamic(&self) -> &Arc<DynamicImage> {
        self.decode_dynamic()
    }

    /// Access the ICC profile, if any.
    pub fn icc(&self) -> Option<&Bytes> {
        self.0.icc.as_ref()
    }

    /// Attempts to decode source rows `[y0, y1)` (clamped to the image's
    /// height) directly as tightly packed RGBA8, without ever
    /// materializing the whole decoded image.
    ///
    /// Returns `None` if the image doesn't qualify for this fast path --
    /// only non-interlaced, 8-bit-per-channel RGB/RGBA PNGs with no
    /// EXIF-driven rotation do. Callers should fall back to
    /// [`Self::dynamic`] in that case.
    ///
    /// This is meant for band-aware rendering of a large raster image
    /// (e.g. a full-bleed poster background): decoding rows before `y0`
    /// is unavoidable (PNG's row-prediction filters require sequential
    /// decompression from the start of the image), but they're discarded
    /// immediately rather than retained, so memory stays bounded to the
    /// requested row range rather than the whole image.
    pub fn decode_rgba_row_range(&self, y0: u32, y1: u32) -> Option<Vec<u8>> {
        if self.0.exif_rotation.is_some() {
            return None;
        }
        if !matches!(self.0.format, RasterFormat::Exchange(ExchangeFormat::Png)) {
            return None;
        }

        let mut reader = png_reader(&self.0.data).ok()?;
        let (width, height) = {
            let info = reader.info();
            if info.interlaced {
                return None;
            }
            (info.width, info.height)
        };

        let (color_type, bit_depth) = reader.output_color_type();
        if bit_depth != png::BitDepth::Eight {
            return None;
        }
        let channels = match color_type {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            _ => return None,
        };

        let y1 = y1.min(height);
        if y0 >= y1 {
            return Some(Vec::new());
        }

        let mut out = vec![0u8; width as usize * (y1 - y0) as usize * 4];
        let mut row = 0u32;
        while row < y1 {
            let Some(data) = reader.next_row().ok()? else { break };
            if row >= y0 {
                let start = (row - y0) as usize * width as usize * 4;
                let dest = &mut out[start..start + width as usize * 4];
                if channels == 4 {
                    dest.copy_from_slice(data.data());
                } else {
                    for (src, dst) in
                        data.data().chunks_exact(3).zip(dest.chunks_exact_mut(4))
                    {
                        dst[..3].copy_from_slice(src);
                        dst[3] = 255;
                    }
                }
            }
            row += 1;
        }

        Some(out)
    }
}

impl Hash for RasterImageInner {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // The image is fully defined by data, format, and ICC profile.
        self.data.hash(state);
        self.format.hash(state);
        self.icc.hash(state);
    }
}

/// A raster graphics format.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub enum RasterFormat {
    /// A format typically used in image exchange.
    Exchange(ExchangeFormat),
    /// A format of raw pixel data.
    Pixel(PixelFormat),
}

impl From<ExchangeFormat> for RasterFormat {
    fn from(format: ExchangeFormat) -> Self {
        Self::Exchange(format)
    }
}

impl From<PixelFormat> for RasterFormat {
    fn from(format: PixelFormat) -> Self {
        Self::Pixel(format)
    }
}

cast! {
    RasterFormat,
    self => match self {
        Self::Exchange(v) => v.into_value(),
        Self::Pixel(v) => v.into_value(),
    },
    v: ExchangeFormat => Self::Exchange(v),
    v: PixelFormat => Self::Pixel(v),
}

/// A raster format typically used in image exchange, with efficient encoding.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Cast)]
pub enum ExchangeFormat {
    /// Raster format for illustrations and transparent graphics.
    Png,
    /// Lossy raster format suitable for photos.
    Jpg,
    /// Raster format that is typically used for short animated clips. Typst can
    /// load GIFs, but they will become static.
    Gif,
    /// Raster format that supports both lossy and lossless compression.
    Webp,
}

impl ExchangeFormat {
    /// Try to detect the format of data in a buffer.
    pub fn detect(data: &[u8]) -> Option<Self> {
        guess_format(data).ok().and_then(|format| format.try_into().ok())
    }
}

impl From<ExchangeFormat> for image::ImageFormat {
    fn from(format: ExchangeFormat) -> Self {
        match format {
            ExchangeFormat::Png => image::ImageFormat::Png,
            ExchangeFormat::Jpg => image::ImageFormat::Jpeg,
            ExchangeFormat::Gif => image::ImageFormat::Gif,
            ExchangeFormat::Webp => image::ImageFormat::WebP,
        }
    }
}

impl TryFrom<image::ImageFormat> for ExchangeFormat {
    type Error = EcoString;

    fn try_from(format: image::ImageFormat) -> StrResult<Self> {
        Ok(match format {
            image::ImageFormat::Png => ExchangeFormat::Png,
            image::ImageFormat::Jpeg => ExchangeFormat::Jpg,
            image::ImageFormat::Gif => ExchangeFormat::Gif,
            image::ImageFormat::WebP => ExchangeFormat::Webp,
            _ => bail!("format not yet supported"),
        })
    }
}

/// Information that is needed to understand a pixmap buffer.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct PixelFormat {
    /// The channel encoding.
    pub encoding: PixelEncoding,
    /// The pixel width.
    pub width: u32,
    /// The pixel height.
    pub height: u32,
}

/// Determines the channel encoding of raw pixel data.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Cast)]
pub enum PixelEncoding {
    /// Three 8-bit channels: Red, green, blue.
    Rgb8,
    /// Four 8-bit channels: Red, green, blue, alpha.
    Rgba8,
    /// One 8-bit channel.
    Luma8,
    /// Two 8-bit channels: Luma and alpha.
    Lumaa8,
}

cast! {
    PixelFormat,
    self => Value::Dict(self.into()),
    mut dict: Dict => {
        let format = Self {
            encoding: dict.take("encoding")?.cast()?,
            width: dict.take("width")?.cast()?,
            height: dict.take("height")?.cast()?,
        };
        dict.finish(&["encoding", "width", "height"])?;
        format
    }
}

impl From<PixelFormat> for Dict {
    fn from(format: PixelFormat) -> Self {
        dict! {
            "encoding" => format.encoding,
            "width" => format.width,
            "height" => format.height,
        }
    }
}

/// Opens a `png` crate reader positioned right after the header (before any
/// pixel data is decoded), with the same transformations `image`'s own PNG
/// decoder uses, so `output_color_type` and row data match what
/// `image::DynamicImage::from_decoder` would eventually produce.
fn png_reader(
    data: &Bytes,
) -> Result<png::Reader<io::Cursor<&Bytes>>, png::DecodingError> {
    let mut decoder = png::Decoder::new(io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND);
    decoder.read_info()
}

/// Reads a PNG's header and validates that the rest of the image data
/// decodes successfully, without ever materializing a full decoded buffer:
/// each row is decoded and discarded, so peak memory here is bounded by a
/// single row rather than the whole image, while still failing fast (like
/// the eager formats in `new_impl`) on a malformed file. Returns
/// `(width, height, has_alpha, icc)`.
fn validate_png(
    data: &Bytes,
    icc: Smart<Bytes>,
) -> StrResult<(u32, u32, bool, Option<Bytes>)> {
    let mut reader = png_reader(data).map_err(png_error_message)?;

    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    Limits::default()
        .check_dimensions(width, height)
        .map_err(|_| EcoString::from("file is too large"))?;

    let icc = icc.custom().or_else(|| {
        reader
            .info()
            .icc_profile
            .as_ref()
            .map(|icc| icc.to_vec())
            .filter(|icc| !icc.is_empty())
            .map(Bytes::new)
    });

    let (color_type, _) = reader.output_color_type();
    let has_alpha =
        matches!(color_type, png::ColorType::GrayscaleAlpha | png::ColorType::Rgba);

    // Stream through (and discard) the rest of the rows to confirm the
    // whole image decodes without error.
    while reader.next_row().map_err(png_error_message)?.is_some() {}

    Ok((width, height, has_alpha, icc))
}

/// Try to get the rotation from the EXIF metadata.
fn exif_rotation(exif: &exif::Exif) -> Option<u32> {
    exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?
        .value
        .get_uint(0)
}

/// Apply an EXIF rotation to a dynamic image.
fn apply_rotation(image: &mut DynamicImage, rotation: u32) {
    use image::imageops as ops;
    match rotation {
        2 => ops::flip_horizontal_in_place(image),
        3 => ops::rotate180_in_place(image),
        4 => ops::flip_vertical_in_place(image),
        5 => {
            ops::flip_horizontal_in_place(image);
            *image = image.rotate270();
        }
        6 => *image = image.rotate90(),
        7 => {
            ops::flip_horizontal_in_place(image);
            *image = image.rotate90();
        }
        8 => *image = image.rotate270(),
        _ => {}
    }
}

/// Try to determine the DPI (dots per inch) of the image.
///
/// This is guaranteed to be a positive value, or `None` if invalid or
/// unspecified.
fn determine_dpi(data: &[u8], exif: Option<&exif::Exif>) -> Option<f64> {
    // Try to extract the DPI from the EXIF metadata. If that doesn't yield
    // anything, fall back to specialized procedures for extracting JPEG or PNG
    // DPI metadata. GIF does not have any.
    exif.and_then(exif_dpi)
        .or_else(|| jpeg_dpi(data))
        .or_else(|| png_dpi(data))
        .filter(|&dpi| dpi > 0.0)
}

/// Try to get the DPI from the EXIF metadata.
fn exif_dpi(exif: &exif::Exif) -> Option<f64> {
    let axis = |tag| {
        let dpi = exif.get_field(tag, exif::In::PRIMARY)?;
        let exif::Value::Rational(rational) = &dpi.value else { return None };
        Some(rational.first()?.to_f64())
    };

    [axis(exif::Tag::XResolution), axis(exif::Tag::YResolution)]
        .into_iter()
        .flatten()
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal))
}

/// Tries to extract the DPI from raw JPEG data (by inspecting the JFIF APP0
/// section).
fn jpeg_dpi(data: &[u8]) -> Option<f64> {
    let validate_at = |index: usize, expect: &[u8]| -> Option<()> {
        data.get(index..)?.starts_with(expect).then_some(())
    };
    let u16_at = |index: usize| -> Option<u16> {
        data.get(index..index + 2)?.try_into().ok().map(u16::from_be_bytes)
    };

    validate_at(0, b"\xFF\xD8\xFF\xE0\0")?;
    validate_at(6, b"JFIF\0")?;
    validate_at(11, b"\x01")?;

    let len = u16_at(4)?;
    if len < 16 {
        return None;
    }

    let units = *data.get(13)?;
    let x = u16_at(14)?;
    let y = u16_at(16)?;
    let dpu = x.max(y) as f64;

    Some(match units {
        1 => dpu,        // already inches
        2 => dpu * 2.54, // cm -> inches
        _ => return None,
    })
}

/// Tries to extract the DPI from raw PNG data.
fn png_dpi(mut data: &[u8]) -> Option<f64> {
    let mut decoder = png::StreamingDecoder::new();
    loop {
        let (consumed, event) = decoder.update(data, None).ok()?;
        match event {
            // Bail as soon as there is anything data-like.
            png::Decoded::ChunkBegin(_, png::chunk::IDAT)
            | png::Decoded::ImageData
            | png::Decoded::ImageDataFlushed => break,
            _ => {}
        }
        data = data.get(consumed..)?;
        if consumed == 0 {
            break;
        }
    }

    let dims = decoder.info().and_then(|i| i.pixel_dims)?;
    let dpu = dims.xppu.max(dims.yppu) as f64;
    match dims.unit {
        png::Unit::Meter => Some(dpu * 0.0254), // meter -> inches
        png::Unit::Unspecified => None,
    }
}

/// Format the user-facing raster graphic decoding error message.
fn format_image_error(error: image::ImageError) -> EcoString {
    match error {
        image::ImageError::Limits(_) => "file is too large".into(),
        err => eco_format!("failed to decode image ({err})"),
    }
}

/// Format a `png`-crate decoding error the same way `format_image_error`
/// would format the equivalent error from `image`'s own PNG decoder (which
/// wraps it in an `ImageError::Decoding` with a "Format error decoding
/// Png: " prefix) -- so the two decode paths produce identical diagnostics.
fn png_error_message(error: png::DecodingError) -> EcoString {
    match error {
        png::DecodingError::LimitsExceeded => "file is too large".into(),
        err @ png::DecodingError::Format(_) => {
            eco_format!("failed to decode image (Format error decoding Png: {err})")
        }
        err => eco_format!("failed to decode image ({err})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_dpi() {
        #[track_caller]
        fn test(path: &str, format: ExchangeFormat, dpi: f64) {
            let data = typst_dev_assets::get(path).unwrap();
            let bytes = Bytes::new(data);
            let image = RasterImage::plain(bytes, format).unwrap();
            assert_eq!(image.dpi().map(f64::round), Some(dpi));
        }

        test("images/f2t.jpg", ExchangeFormat::Jpg, 220.0);
        test("images/tiger.jpg", ExchangeFormat::Jpg, 72.0);
        test("images/graph.png", ExchangeFormat::Png, 144.0);
    }

    /// The fast row-range decode must produce byte-identical output to the
    /// fully decoded image, for a representative sub-range of rows (start,
    /// middle, end, and the whole image) in both an RGB and an RGBA PNG.
    #[test]
    fn test_row_range_matches_full_decode() {
        #[track_caller]
        fn test(path: &str) {
            let data = typst_dev_assets::get(path).unwrap();
            let image =
                RasterImage::plain(Bytes::new(data), ExchangeFormat::Png).unwrap();
            let full = image.dynamic().to_rgba8();
            let width = image.width();
            let height = image.height();

            let ranges: &[(u32, u32)] = &[
                (0, 1),
                (0, height),
                (height / 2, height / 2 + 3),
                (height - 1, height),
                (height.saturating_sub(2), height + 10), // clamped past the end
            ];
            for &(y0, y1) in ranges {
                let region = image
                    .decode_rgba_row_range(y0, y1)
                    .unwrap_or_else(|| panic!("expected fast path for {path}"));
                let y1 = y1.min(height);
                let expected = &full.as_raw()[(y0 as usize * width as usize * 4)
                    ..(y1 as usize * width as usize * 4)];
                assert_eq!(region, expected, "{path}: rows {y0}..{y1}");
            }
        }

        test("images/chart-good.png"); // RGB, no alpha
        test("images/graph.png"); // RGBA
        test("images/small.png"); // palette -> expanded to RGB(A) by EXPAND
    }

    /// Formats/cases the fast path doesn't support must cleanly fall back
    /// (return `None`) rather than produce wrong output.
    #[test]
    fn test_row_range_fallback() {
        // Not a PNG at all.
        let jpg = typst_dev_assets::get("images/tiger.jpg").unwrap();
        let jpg = RasterImage::plain(Bytes::new(jpg), ExchangeFormat::Jpg).unwrap();
        assert!(jpg.decode_rgba_row_range(0, 1).is_none());

        // Grayscale PNG (not RGB/RGBA even after `EXPAND`).
        let gray = typst_dev_assets::get("screenshots/3-advanced-paper.png").unwrap();
        let gray = RasterImage::plain(Bytes::new(gray), ExchangeFormat::Png).unwrap();
        assert!(gray.decode_rgba_row_range(0, 1).is_none());
    }
}
