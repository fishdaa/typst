use std::cmp::Ordering;
use std::hash::{Hash, Hasher};
use std::io;
use std::sync::{Arc, Mutex, OnceLock};

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
    /// For a PNG, whether its channels are natively 8 bits per sample
    /// (`None` for other formats, where [`RasterImage::is_native_8bit`]
    /// checks the eagerly-decoded `dynamic` instead, which costs nothing
    /// extra since it's already resident). Also known from the header alone.
    png_is_8bit: Option<bool>,
    /// The fully decoded image, decoded lazily on first access via
    /// [`RasterImage::dynamic`] -- so a caller that only ever needs a region
    /// of the image (see [`RasterImage::decode_rgba_row_range`]) never pays
    /// for the full decode.
    dynamic: OnceLock<Arc<DynamicImage>>,
    exif_rotation: Option<u32>,
    icc: Option<Bytes>,
    dpi: Option<f64>,
    /// A decoder positioned partway through the image, reused across
    /// successive [`RasterImage::decode_rgba_row_range`] calls that request
    /// increasing row ranges (the common case: a band-rendered page decodes
    /// each image top-to-bottom, one row range per band). Without this,
    /// every call would restart decoding from row 0 -- since PNG rows are
    /// filtered against the previous row, there's no way to seek directly to
    /// a later row -- making a full band-by-band render of an image
    /// `O(bands * height)` instead of `O(height)`.
    row_cursor: Mutex<Option<RowCursor>>,
}

/// A `png` reader paused after decoding up to (but not including) `next_row`.
struct RowCursor {
    reader: png::Reader<io::Cursor<Bytes>>,
    next_row: u32,
    width: u32,
    channels: u8,
    /// Bytes per channel sample in the *source* row data `next_row()`
    /// hands back (1 for 8-bit, 2 for 16-bit). The decoded output is
    /// always tightly packed RGBA8 regardless of this value.
    bytes_per_sample: u8,
}

impl RowCursor {
    /// Opens a fresh reader positioned at row 0. Returns `None` for anything
    /// [`RasterImage::decode_rgba_row_range`] doesn't support.
    fn new(data: &Bytes) -> Option<Self> {
        let reader = png_reader(data.clone()).ok()?;
        let width = {
            let info = reader.info();
            if info.interlaced {
                return None;
            }
            info.width
        };
        let (color_type, bit_depth) = reader.output_color_type();
        let (bytes_per_sample, channels) = match (bit_depth, color_type) {
            (png::BitDepth::Eight, png::ColorType::Rgb) => (1, 3),
            (png::BitDepth::Eight, png::ColorType::Rgba) => (1, 4),
            (png::BitDepth::Sixteen, png::ColorType::Rgb) => (2, 3),
            (png::BitDepth::Sixteen, png::ColorType::Rgba) => (2, 4),
            _ => return None,
        };
        Some(Self { reader, next_row: 0, width, channels, bytes_per_sample })
    }
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
        let (width, height, has_alpha, icc, dpi, eager_dynamic, png_is_8bit) = match format
        {
            RasterFormat::Exchange(ExchangeFormat::Png) => {
                let (raw_w, raw_h, has_alpha, icc, is_8bit) = validate_png(&data, icc)?;

                let exif = exif::Reader::new()
                    .read_from_container(&mut io::Cursor::new(&data))
                    .ok();

                let (mut width, mut height) = (raw_w, raw_h);
                if let Some(rotation) = exif.as_ref().and_then(exif_rotation) {
                    if matches!(rotation, 5..=8) {
                        std::mem::swap(&mut width, &mut height);
                    }
                    exif_rot = Some(rotation);
                }

                let dpi = determine_dpi(&data, exif.as_ref());
                (width, height, has_alpha, icc, dpi, None, Some(is_8bit))
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

                (width, height, has_alpha, icc, dpi, Some(dynamic), None)
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
                    None,
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
            png_is_8bit,
            dynamic: dynamic_cell,
            exif_rotation: exif_rot,
            icc,
            dpi,
            row_cursor: Mutex::new(None),
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

    /// Whether the image's channels are natively 8 bits per sample, i.e.
    /// deriving an 8-bit-per-channel buffer from it involves no lossy
    /// bit-depth reduction.
    ///
    /// Consumers that split the image into separate color/alpha buffers for
    /// re-embedding (e.g. PDF export) use this to decide whether an embedded
    /// ICC profile is still valid for the re-derived buffer, without forcing
    /// a full decode via [`Self::dynamic`] just to check: for PNG this is
    /// already known from the header alone (see [`Self::decode_rgba_row_range`],
    /// which similarly avoids a full decode).
    pub fn is_native_8bit(&self) -> bool {
        self.0.png_is_8bit.unwrap_or_else(|| {
            matches!(
                self.dynamic().as_ref(),
                DynamicImage::ImageLuma8(_)
                    | DynamicImage::ImageLumaA8(_)
                    | DynamicImage::ImageRgb8(_)
                    | DynamicImage::ImageRgba8(_)
            )
        })
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
    /// only non-interlaced 8-bit- or 16-bit-per-channel RGB/RGBA PNGs with
    /// no EXIF-driven rotation do. Callers should fall back to
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

        let mut guard = self.0.row_cursor.lock().unwrap();

        // Reuse the decoder if it's already positioned at or before `y0`
        // (the common case, since bands are rendered top-to-bottom).
        // Otherwise -- first call, or a request that rewinds, e.g. the same
        // image placed twice on a page -- start over from row 0.
        let cursor = match guard.take() {
            Some(cursor) if cursor.next_row <= y0 => cursor,
            _ => RowCursor::new(&self.0.data)?,
        };
        let RowCursor { mut reader, mut next_row, width, channels, bytes_per_sample } =
            cursor;

        let height = reader.info().height;
        let y1 = y1.min(height);
        if y0 >= y1 {
            *guard =
                Some(RowCursor { reader, next_row, width, channels, bytes_per_sample });
            return Some(Vec::new());
        }

        let mut out = vec![0_u8; width as usize * (y1 - y0) as usize * 4];
        while next_row < y1 {
            let Some(data) = reader.next_row().ok()? else { break };
            if next_row >= y0 {
                let start = (next_row - y0) as usize * width as usize * 4;
                let dest = &mut out[start..start + width as usize * 4];
                match (bytes_per_sample, channels) {
                    (1, 4) => dest.copy_from_slice(data.data()),
                    (1, 3) => {
                        for (src, dst) in
                            data.data().chunks_exact(3).zip(dest.chunks_exact_mut(4))
                        {
                            dst[..3].copy_from_slice(src);
                            dst[3] = 255;
                        }
                    }
                    (2, 4) => {
                        for (src, dst) in
                            data.data().chunks_exact(8).zip(dest.chunks_exact_mut(4))
                        {
                            for i in 0..4 {
                                dst[i] = sample16_to_8(src[i * 2], src[i * 2 + 1]);
                            }
                        }
                    }
                    (2, 3) => {
                        for (src, dst) in
                            data.data().chunks_exact(6).zip(dest.chunks_exact_mut(4))
                        {
                            for i in 0..3 {
                                dst[i] = sample16_to_8(src[i * 2], src[i * 2 + 1]);
                            }
                            dst[3] = 255;
                        }
                    }
                    _ => unreachable!("RowCursor::new only yields these combinations"),
                }
            }
            next_row += 1;
        }

        *guard = Some(RowCursor { reader, next_row, width, channels, bytes_per_sample });
        Some(out)
    }

    /// Visits source rows `[y0, y1)` one at a time as tightly packed RGBA8.
    ///
    /// This is useful when a consumer can process a row immediately instead
    /// of retaining the complete decoded band. In particular, a native-size
    /// alpha background can be blended directly into the render canvas with
    /// only one source row resident.
    pub fn for_each_rgba_row<F>(&self, y0: u32, y1: u32, mut f: F) -> Option<()>
    where
        F: FnMut(u32, &[u8]),
    {
        for y in y0..y1 {
            let row = self.decode_rgba_row_range(y, y + 1)?;
            f(y, &row);
        }
        Some(())
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
        let format = guess_format(data).ok()?;
        format.try_into().ok()
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
    data: Bytes,
) -> Result<png::Reader<io::Cursor<Bytes>>, png::DecodingError> {
    let mut decoder = png::Decoder::new(io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND);
    decoder.read_info()
}

/// Converts a big-endian 16-bit PNG channel sample to 8 bits, matching
/// `image`'s own `FromPrimitive<u16> for u8` exactly (`(v * 255 + 127.5) /
/// 65535` rounded to nearest, computed here as `(v + 128) / 257` in integer
/// arithmetic) so [`RasterImage::decode_rgba_row_range`]'s output stays
/// byte-identical to [`RasterImage::dynamic`]`().to_rgba8()`. This is *not*
/// `(v >> 8) as u8` -- that truncates instead of rounding and would produce
/// different output for roughly half of all input values.
fn sample16_to_8(hi: u8, lo: u8) -> u8 {
    let v = u16::from_be_bytes([hi, lo]) as u32;
    ((v + 128) / 257) as u8
}

/// Reads a PNG's header and validates that the rest of the image data
/// decodes successfully, without ever materializing a full decoded buffer:
/// each row is decoded and discarded, so peak memory here is bounded by a
/// single row rather than the whole image, while still failing fast (like
/// the eager formats in `new_impl`) on a malformed file. Returns
/// `(width, height, has_alpha, icc, is_8bit)`.
fn validate_png(
    data: &Bytes,
    icc: Smart<Bytes>,
) -> StrResult<(u32, u32, bool, Option<Bytes>, bool)> {
    let mut reader = png_reader(data.clone()).map_err(png_error_message)?;

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

    let (color_type, bit_depth) = reader.output_color_type();
    let has_alpha =
        matches!(color_type, png::ColorType::GrayscaleAlpha | png::ColorType::Rgba);
    let is_8bit = bit_depth == png::BitDepth::Eight;

    // Stream through (and discard) the rest of the rows to confirm the
    // whole image decodes without error.
    while reader.next_row().map_err(png_error_message)?.is_some() {}

    Ok((width, height, has_alpha, icc, is_8bit))
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

    /// Encodes a small in-memory PNG with varied, non-trivial 16-bit sample
    /// values (not just 0/max), to exercise `sample16_to_8`'s rounding
    /// across its range rather than only its endpoints.
    fn encode_png16(width: u32, height: u32, color_type: png::ColorType) -> Vec<u8> {
        let channels = match color_type {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            _ => unreachable!("test only uses Rgb/Rgba"),
        };
        let mut data = Vec::with_capacity((width * height * channels * 2) as usize);
        for i in 0..(width * height * channels) {
            // A varied, deterministic sequence covering low/mid/high values.
            let v = ((i.wrapping_mul(2654435761)) % 0x0001_0000) as u16;
            data.extend_from_slice(&v.to_be_bytes());
        }

        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(color_type);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&data).unwrap();
        }
        out
    }

    /// The fast path must handle 16-bit PNGs, converting each channel
    /// sample down to 8 bits with the exact same rounding `image`'s own
    /// `FromPrimitive<u16> for u8` uses, so output stays byte-identical to
    /// the full decode.
    #[test]
    fn test_row_range_matches_full_decode_16bit() {
        #[track_caller]
        fn test(color_type: png::ColorType) {
            let data = encode_png16(4, 5, color_type);
            let image = RasterImage::plain(Bytes::new(data), ExchangeFormat::Png)
                .unwrap_or_else(|err| panic!("{color_type:?}: {err:?}"));
            let full = image.dynamic().to_rgba8();
            let width = image.width();
            let height = image.height();

            let region = image
                .decode_rgba_row_range(0, height)
                .unwrap_or_else(|| panic!("expected fast path for {color_type:?}"));
            assert_eq!(
                region,
                &full.as_raw()[..(width * height * 4) as usize],
                "{color_type:?}"
            );

            // A mid-image sub-range too, to exercise the row-cursor's
            // partial-range path, not just the whole-image case.
            let region = image.decode_rgba_row_range(2, 4).unwrap();
            let expected = &full.as_raw()
                [(2 * width * 4) as usize..(4 * width * 4) as usize];
            assert_eq!(region, expected, "{color_type:?}: rows 2..4");
        }

        test(png::ColorType::Rgb);
        test(png::ColorType::Rgba);
    }

    /// Pins the 16-bit-to-8-bit channel rounding formula independent of a
    /// full image round-trip: `image`'s own `FromPrimitive<u16> for u8` is
    /// `round(v * 255 / 65535)`, not `(v >> 8) as u8` truncation.
    #[test]
    fn test_sample16_to_8() {
        assert_eq!(sample16_to_8(0x00, 0x00), 0);
        assert_eq!(sample16_to_8(0xFF, 0xFF), 255);
        assert_eq!(sample16_to_8(0x80, 0x00), 128); // 32768 -> 128
        assert_eq!(sample16_to_8(0x01, 0x00), 1); // 256 -> 1 (agrees with >>8)

        // A value where rounding and truncation genuinely disagree:
        // 200 -> round(200 * 255 / 65535) = round(0.778) = 1, whereas
        // truncating `200 >> 8 = 0`. Confirms the formula rounds rather
        // than truncates, matching `image`'s own conversion exactly.
        assert_eq!(sample16_to_8(0x00, 0xC8), 1);
        assert_ne!(sample16_to_8(0x00, 0xC8), (200_u16 >> 8) as u8);
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

        // Note: an interlaced-PNG case is intentionally not covered here --
        // the `png` crate's `Writer::write_image_data` doesn't support
        // writing Adam7-interlaced data (it always lays out `data` as plain
        // consecutive rows regardless of `Info::interlaced`), so
        // synthesizing a valid interlaced fixture in-process isn't
        // straightforward. The qualification check itself
        // (`RowCursor::new`'s `if info.interlaced { return None }`) is
        // untouched by this change.
    }
}
