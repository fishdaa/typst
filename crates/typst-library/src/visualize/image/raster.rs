use std::cmp::Ordering;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::io;
use std::sync::atomic::{self, AtomicU32};
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
    /// Shared RGBA8 conversion for renderers that cannot stream source rows.
    rgba8: OnceLock<Arc<DynamicImage>>,
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
    /// A floor, in rows, on how much of the decoded tail `row_cursor` keeps
    /// behind itself. Raised through [`RasterImage::reserve_retained_rows`]
    /// by a caller that is about to make row-range requests out of order.
    retain_rows: AtomicU32,
    /// How many times a decoder has been opened for `row_cursor`. Since a
    /// PNG can only be decompressed from the start, each start past the
    /// first re-does all the work up to the requested row -- so this is the
    /// signal that row-range requests are defeating the cursor. Exposed via
    /// [`RasterImage::decoder_starts`] so that can be asserted against.
    decoder_starts: AtomicU32,
}

/// A [`std::io::Read`] adapter over [`Bytes`] that releases the pages it has
/// already consumed (see [`Bytes::drop_behind`]).
///
/// A PNG's pixel data can only be decompressed front-to-back, so a decoder
/// walks the file exactly once, in order. Without this, decoding a large
/// memory-mapped asset leaves the entire file resident for the rest of the
/// export -- which, for a poster-sized background, dominates peak memory
/// however carefully the render path bounds its own buffers, because a
/// memory-constrained cgroup charges those clean pages just like heap
/// memory.
struct PagedSource {
    data: Bytes,
    pos: usize,
    /// How far this reader has already released, so each pass over the same
    /// bytes releases what *it* read rather than relying on some earlier
    /// pass having done so.
    released: usize,
}

/// How far behind the read position [`PagedSource`] keeps resident. The `png`
/// crate copies out of `read`'s buffer rather than retaining it, so this is
/// only a safety margin.
const KEEP_RESIDENT_BEHIND: usize = 64 * 1024;

/// How much [`PagedSource`] lets accumulate before releasing it, so a
/// decoder reading a few kilobytes at a time doesn't pay a syscall per read.
const RELEASE_CHUNK: usize = 4 * 1024 * 1024;

impl PagedSource {
    fn new(data: Bytes) -> Self {
        Self { data, pos: 0, released: 0 }
    }

    /// Advances the read position, releasing what is now well behind it.
    fn advance(&mut self, amount: usize) {
        self.pos = (self.pos + amount).min(self.data.len());

        let target = self.pos.saturating_sub(KEEP_RESIDENT_BEHIND);
        if target >= self.released + RELEASE_CHUNK {
            self.data.release(self.released..target);
            self.released = target;
        }
    }
}

impl io::Read for PagedSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let slice = &self.data.as_slice()[self.pos..];
        let n = slice.len().min(buf.len());
        buf[..n].copy_from_slice(&slice[..n]);
        self.advance(n);
        Ok(n)
    }
}

impl io::BufRead for PagedSource {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Ok(&self.data.as_slice()[self.pos..])
    }

    fn consume(&mut self, amount: usize) {
        self.advance(amount);
    }
}

impl io::Seek for PagedSource {
    /// Seeking backwards is allowed and always correct: a released page is
    /// transparently faulted back in (see [`Bytes::drop_behind`]), it just
    /// costs a fault. The decoder only seeks within the header region in
    /// practice.
    fn seek(&mut self, from: io::SeekFrom) -> io::Result<u64> {
        let len = self.data.len() as i64;
        let target = match from {
            io::SeekFrom::Start(offset) => offset as i64,
            io::SeekFrom::End(offset) => len + offset,
            io::SeekFrom::Current(offset) => self.pos as i64 + offset,
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot seek before the start of the data",
            ));
        }
        self.pos = (target as usize).min(self.data.len());
        // Anything after the new position may be read again, so allow it to
        // be released again too.
        self.released = self.released.min(self.pos);
        Ok(self.pos as u64)
    }
}

/// Bounds on [`RowCursor::retained`]: enough rows to absorb a resize
/// filter's kernel support at any realistic scale factor, while never
/// holding a meaningful fraction of a large image.
const RETAIN_MAX_ROWS: usize = 128;
const RETAIN_MAX_BYTES: usize = 4 * 1024 * 1024;

/// The ceiling on a window requested through
/// [`RasterImage::reserve_retained_rows`], which asks for more than the
/// default bounds above. That request is derived from a band's height, which
/// a caller already sizes to its memory budget, so this only guards against
/// a nonsensical value.
const RETAIN_RESERVED_MAX_BYTES: usize = 64 * 1024 * 1024;

/// A `png` reader paused after decoding up to (but not including)
/// `next_row`, plus a bounded tail of the rows it already handed out.
struct RowCursor {
    reader: png::Reader<PagedSource>,
    next_row: u32,
    width: u32,
    channels: u8,
    /// Bytes per channel sample in the *source* row data `next_row()`
    /// hands back (1 for 8-bit, 2 for 16-bit). Decoded output is tightly
    /// packed 8-bit channels regardless of this value.
    bytes_per_sample: u8,
    /// Raw (unconverted) source rows that were already decoded, oldest
    /// first, so a request starting slightly *before* `next_row` can be
    /// served without restarting decompression from row 0.
    ///
    /// The resampling render path asks for *overlapping* row ranges: it
    /// expands each band by the resize filter's kernel support on both
    /// sides, so band N+1 begins a few rows above where band N stopped.
    /// Since PNG rows are filtered against their predecessor there is no way
    /// to seek backwards, so without this tail that small overlap forced a
    /// full restart, making a banded export of one image cost
    /// `O(bands * height)` row decodes instead of `O(height)`.
    retained: VecDeque<Vec<u8>>,
    /// The row index of `retained.front()`. Maintained so that
    /// `retained_from + retained.len() == next_row`.
    retained_from: u32,
    /// A spare row buffer, recycled between `next_row()` calls so that
    /// retaining rows doesn't allocate per row.
    spare: Vec<u8>,
}

impl RowCursor {
    /// Opens a fresh reader positioned at row 0. Returns `None` for anything
    /// [`RasterImage::decode_row_range`] doesn't support.
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
            (png::BitDepth::Eight, png::ColorType::Grayscale) => (1, 1),
            (png::BitDepth::Eight, png::ColorType::GrayscaleAlpha) => (1, 2),
            (png::BitDepth::Eight, png::ColorType::Rgb) => (1, 3),
            (png::BitDepth::Eight, png::ColorType::Rgba) => (1, 4),
            (png::BitDepth::Sixteen, png::ColorType::Grayscale) => (2, 1),
            (png::BitDepth::Sixteen, png::ColorType::GrayscaleAlpha) => (2, 2),
            (png::BitDepth::Sixteen, png::ColorType::Rgb) => (2, 3),
            (png::BitDepth::Sixteen, png::ColorType::Rgba) => (2, 4),
            _ => return None,
        };
        Some(Self {
            reader,
            next_row: 0,
            width,
            channels,
            bytes_per_sample,
            retained: VecDeque::new(),
            retained_from: 0,
            spare: Vec::new(),
        })
    }

    /// Adds a just-decoded row to the retained tail, returning a buffer that
    /// is free for reuse (the evicted row, or a fresh empty one).
    ///
    /// `reserved` is a floor on the window size requested through
    /// [`RasterImage::reserve_retained_rows`]; the default bounds apply when
    /// it is zero.
    fn push_retained(&mut self, row: Vec<u8>, reserved: u32) -> Vec<u8> {
        let row_len = row.len().max(1);
        let default_rows = RETAIN_MAX_ROWS.min((RETAIN_MAX_BYTES / row_len).max(1));
        let reserved_rows =
            (reserved as usize).min((RETAIN_RESERVED_MAX_BYTES / row_len).max(1));
        let max_rows = default_rows.max(reserved_rows);
        self.retained.push_back(row);
        if self.retained.len() > max_rows {
            self.retained_from += 1;
            self.retained.pop_front().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Visits rows `[y0, y1)` as tightly packed `out_channels`-channel rows,
    /// serving what the retained tail still holds and decoding the rest.
    ///
    /// The caller must have checked that `y0 >= self.retained_from`.
    fn visit(
        &mut self,
        y0: u32,
        y1: u32,
        out_channels: u8,
        reserved: u32,
        f: &mut dyn FnMut(u32, &[u8]),
    ) -> Option<()> {
        let mut scratch = vec![0_u8; self.width as usize * out_channels as usize];

        // Rows that were already decoded and are still retained.
        for y in y0..y1.min(self.next_row) {
            let row = self.retained.get((y - self.retained_from) as usize)?;
            convert_row(
                row,
                &mut scratch,
                self.bytes_per_sample,
                self.channels,
                out_channels,
            );
            f(y, &scratch);
        }

        // Decode forward, retaining each row on the way past. The row is
        // copied out of the decoder before being retained, because holding
        // the decoder's borrow would conflict with mutating `self`.
        while self.next_row < y1 {
            let mut buf = std::mem::take(&mut self.spare);
            buf.clear();
            match self.reader.next_row() {
                Ok(Some(row)) => buf.extend_from_slice(row.data()),
                Ok(None) => {
                    self.spare = buf;
                    break;
                }
                Err(_) => {
                    self.spare = buf;
                    return None;
                }
            }

            let y = self.next_row;
            if y >= y0 {
                convert_row(
                    &buf,
                    &mut scratch,
                    self.bytes_per_sample,
                    self.channels,
                    out_channels,
                );
                f(y, &scratch);
            }
            self.next_row += 1;
            self.spare = self.push_retained(buf, reserved);
        }

        Some(())
    }
}

/// Converts one raw PNG row into `dst`, tightly packed with `out_channels`
/// 8-bit channels. Grayscale samples are replicated into RGB; sources
/// without alpha get a fully opaque alpha in a 4-channel destination.
fn convert_row(
    src: &[u8],
    dst: &mut [u8],
    bytes_per_sample: u8,
    channels: u8,
    out_channels: u8,
) {
    match (bytes_per_sample, channels, out_channels) {
        (1 | 2, 1 | 2, 3 | 4) => {
            let sample_bytes = bytes_per_sample as usize;
            for (s, d) in src
                .chunks_exact(channels as usize * sample_bytes)
                .zip(dst.chunks_exact_mut(out_channels as usize))
            {
                let sample = |offset| {
                    if sample_bytes == 1 {
                        s[offset]
                    } else {
                        sample16_to_8(s[offset], s[offset + 1])
                    }
                };
                d[..3].fill(sample(0));
                if out_channels == 4 {
                    d[3] = if channels == 2 { sample(sample_bytes) } else { 255 };
                }
            }
        }
        // Already exactly the destination layout.
        (1, 4, 4) | (1, 3, 3) => dst.copy_from_slice(&src[..dst.len()]),
        (1, 3, 4) => {
            for (s, d) in src.chunks_exact(3).zip(dst.chunks_exact_mut(4)) {
                d[..3].copy_from_slice(s);
                d[3] = 255;
            }
        }
        (2, 4, 4) => {
            for (s, d) in src.chunks_exact(8).zip(dst.chunks_exact_mut(4)) {
                for i in 0..4 {
                    d[i] = sample16_to_8(s[i * 2], s[i * 2 + 1]);
                }
            }
        }
        (2, 3, 4) => {
            for (s, d) in src.chunks_exact(6).zip(dst.chunks_exact_mut(4)) {
                for i in 0..3 {
                    d[i] = sample16_to_8(s[i * 2], s[i * 2 + 1]);
                }
                d[3] = 255;
            }
        }
        (2, 3, 3) => {
            for (s, d) in src.chunks_exact(6).zip(dst.chunks_exact_mut(3)) {
                for i in 0..3 {
                    d[i] = sample16_to_8(s[i * 2], s[i * 2 + 1]);
                }
            }
        }
        _ => unreachable!("unsupported channel conversion"),
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
        let (width, height, has_alpha, icc, dpi, eager_dynamic, png_is_8bit) =
            match format {
                RasterFormat::Exchange(ExchangeFormat::Png) => {
                    let (raw_w, raw_h, has_alpha, icc, is_8bit) =
                        validate_png(&data, icc)?;

                    // Read through `PagedSource` rather than a plain
                    // cursor: finding out that a PNG carries no EXIF at all
                    // means walking its chunks to the end, and doing that
                    // over a plain cursor pulls the entire file into memory
                    // -- which for a large asset is the single biggest
                    // contribution to peak memory, dwarfing everything the
                    // render path is careful about.
                    let exif = exif::Reader::new()
                        .read_from_container(&mut PagedSource::new(data.clone()))
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
                    ) -> ImageResult<(image::DynamicImage, Option<Bytes>)>
                    {
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
                        PixelEncoding::Rgba8 => {
                            to::<image::Rgba<u8>>(&data, format).into()
                        }
                        PixelEncoding::Luma8 => {
                            to::<image::Luma<u8>>(&data, format).into()
                        }
                        PixelEncoding::Lumaa8 => {
                            to::<image::LumaA<u8>>(&data, format).into()
                        }
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
            rgba8: OnceLock::new(),
            exif_rotation: exif_rot,
            icc,
            dpi,
            row_cursor: Mutex::new(None),
            retain_rows: AtomicU32::new(0),
            decoder_starts: AtomicU32::new(0),
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

    /// Access the fully decoded image with RGBA8 pixels.
    ///
    /// Concurrent render tiles share one conversion. Memoizing the conversion
    /// alone can still compute it concurrently on a cache miss, temporarily
    /// allocating a full image per tile. The once cell also shares an existing
    /// RGBA8 dynamic image without copying its pixels.
    pub fn rgba8(&self) -> &Arc<DynamicImage> {
        self.0.rgba8.get_or_init(|| {
            let dynamic = self.dynamic();
            if dynamic.as_rgba8().is_some() {
                dynamic.clone()
            } else {
                Arc::new(DynamicImage::ImageRgba8(dynamic.to_rgba8()))
            }
        })
    }

    /// Whether the source uses grayscale samples, without decoding PNG pixels.
    pub fn is_grayscale(&self) -> bool {
        if matches!(self.0.format, RasterFormat::Exchange(ExchangeFormat::Png)) {
            return png_reader(self.0.data.clone()).is_ok_and(|reader| {
                matches!(
                    reader.output_color_type().0,
                    png::ColorType::Grayscale | png::ColorType::GrayscaleAlpha
                )
            });
        }
        matches!(
            self.dynamic().color(),
            image::ColorType::L8
                | image::ColorType::La8
                | image::ColorType::L16
                | image::ColorType::La16
        )
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
    /// only non-interlaced PNGs with RGB or grayscale samples and
    /// no EXIF-driven rotation do. Callers should fall back to
    /// [`Self::dynamic`] in that case.
    pub fn decode_rgba_row_range(&self, y0: u32, y1: u32) -> Option<Vec<u8>> {
        self.decode_row_range(y0, y1, 4)
    }

    /// Like [`Self::decode_rgba_row_range`], but with a caller-chosen
    /// channel count: 4 for RGBA8, or 3 for RGB8.
    ///
    /// Asking for 3 channels is only honored for a source that has no alpha
    /// channel of its own (otherwise dropping it would silently change the
    /// pixels), and lets a consumer that doesn't need alpha -- e.g. resizing
    /// an opaque background -- hold 25% less memory per buffer.
    ///
    /// This is meant for band-aware rendering of a large raster image
    /// (e.g. a full-bleed poster background): decoding rows before `y0`
    /// is unavoidable (PNG's row-prediction filters require sequential
    /// decompression from the start of the image), but they're discarded
    /// immediately rather than retained, so memory stays bounded to the
    /// requested row range rather than the whole image.
    pub fn decode_row_range(&self, y0: u32, y1: u32, channels: u8) -> Option<Vec<u8>> {
        let y1 = y1.min(self.0.height);
        let stride = self.0.width as usize * channels as usize;
        let rows = y1.saturating_sub(y0) as usize;
        let mut out = vec![0_u8; rows * stride];
        self.visit_rows(y0, y1, channels, |y, row| {
            let offset = (y - y0) as usize * stride;
            out[offset..offset + stride].copy_from_slice(row);
        })?;
        Some(out)
    }

    /// Opens a decoder positioned at row 0, counting the start.
    fn start_cursor(&self) -> Option<RowCursor> {
        self.0.decoder_starts.fetch_add(1, atomic::Ordering::Relaxed);
        RowCursor::new(&self.0.data)
    }

    /// How many times a PNG decoder has been opened to serve row-range
    /// requests for this image.
    ///
    /// One is the ideal: a decoder can only move forwards, so every
    /// additional start re-decompresses the image from row 0 up to whatever
    /// row was asked for. Consumers that walk the image in order (banded
    /// rendering) should never push this above one; it is exposed so tests
    /// can assert that.
    pub fn decoder_starts(&self) -> u32 {
        self.0.decoder_starts.load(atomic::Ordering::Relaxed)
    }

    /// Asks the row cursor to keep at least `rows` already-decoded rows
    /// behind itself, so that a later request for an earlier row is served
    /// from memory instead of restarting decompression from row 0.
    ///
    /// Row-range requests are cheap only while they arrive in increasing
    /// order, which holds naturally for a band-by-band render. The row tiles
    /// of a single band, however, are rendered concurrently and so reach the
    /// image in an arbitrary order: whichever tile arrives first pulls the
    /// cursor down to its own rows, and every tile above it then asks for
    /// rows the cursor has already passed. Reserving a window as tall as the
    /// band's source rows makes that order irrelevant -- the rows are decoded
    /// exactly once and handed out in whatever order the tiles ask for them.
    ///
    /// The floor only ever rises, and is capped (see
    /// `RETAIN_RESERVED_MAX_BYTES`).
    pub fn reserve_retained_rows(&self, rows: u32) {
        self.0.retain_rows.fetch_max(rows, atomic::Ordering::Relaxed);
    }

    /// Whether [`Self::decode_row_range`] can serve this image with the
    /// given channel count, without decoding any pixels.
    ///
    /// Lets a caller size its buffers for the cheaper channel count up
    /// front instead of discovering only afterwards that it has to fall back
    /// to the fully decoded image.
    pub fn supports_row_range(&self, channels: u8) -> bool {
        if self.0.exif_rotation.is_some() {
            return false;
        }
        if !matches!(self.0.format, RasterFormat::Exchange(ExchangeFormat::Png)) {
            return false;
        }
        if !matches!(channels, 3 | 4) {
            return false;
        }

        // Opens the decoder if it isn't open yet (reading only the header),
        // and then leaves it exactly where it was -- in particular this must
        // never rewind a cursor that is already positioned mid-image, since
        // that would restart decompression from row 0.
        let mut guard = self.0.row_cursor.lock().unwrap();
        let cursor = match guard.take() {
            Some(cursor) => cursor,
            None => match self.start_cursor() {
                Some(cursor) => cursor,
                None => return false,
            },
        };

        // Three channels are only available for a source that has no alpha
        // channel of its own.
        let supported = channels == 4 || matches!(cursor.channels, 1 | 3);
        *guard = Some(cursor);
        supported
    }

    /// Visits source rows `[y0, y1)` one at a time as tightly packed RGBA8.
    ///
    /// This is useful when a consumer can process a row immediately instead
    /// of retaining the complete decoded band. In particular, a native-size
    /// alpha background can be blended directly into the render canvas with
    /// only one source row resident.
    pub fn for_each_rgba_row<F>(&self, y0: u32, y1: u32, f: F) -> Option<()>
    where
        F: FnMut(u32, &[u8]),
    {
        self.visit_rows(y0, y1.min(self.0.height), 4, f)
    }

    /// The shared implementation of [`Self::decode_row_range`] and
    /// [`Self::for_each_rgba_row`]: holds the decoder's lock across the
    /// whole range and converts one row at a time into a single reusable
    /// buffer, so neither the lock nor an allocation is paid per row.
    fn visit_rows<F>(&self, y0: u32, y1: u32, out_channels: u8, mut f: F) -> Option<()>
    where
        F: FnMut(u32, &[u8]),
    {
        if self.0.exif_rotation.is_some() {
            return None;
        }
        if !matches!(self.0.format, RasterFormat::Exchange(ExchangeFormat::Png)) {
            return None;
        }
        if !matches!(out_channels, 3 | 4) {
            return None;
        }

        let mut guard = self.0.row_cursor.lock().unwrap();

        // Reuse the decoder when it is positioned at or before `y0`, or when
        // the rows in between are still in its retained tail (see
        // `RowCursor::retained`). Otherwise -- the first call, or a rewind
        // past the tail, e.g. the same image placed twice on one page --
        // start over from row 0.
        let mut cursor = match guard.take() {
            Some(cursor) if y0 >= cursor.retained_from => cursor,
            _ => self.start_cursor()?,
        };

        // Dropping a real alpha channel is not this function's call to make.
        if out_channels == 3 && !matches!(cursor.channels, 1 | 3) {
            *guard = Some(cursor);
            return None;
        }

        let reserved = self.0.retain_rows.load(atomic::Ordering::Relaxed);
        let result = if y0 < y1 {
            cursor.visit(y0, y1, out_channels, reserved, &mut f)
        } else {
            Some(())
        };
        *guard = Some(cursor);
        result
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
fn png_reader(data: Bytes) -> Result<png::Reader<PagedSource>, png::DecodingError> {
    let mut decoder = png::Decoder::new(PagedSource::new(data));
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

    // Confirm the rest of the file is intact. Small images are inflated
    // outright, exactly like the eager formats in `new_impl`; large ones get
    // the much cheaper structural check (see
    // `EAGER_VALIDATION_MAX_DECODED_BYTES`).
    let channels = match color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => 4,
    };
    let bytes_per_sample = if is_8bit { 1 } else { 2 };
    let decoded_bytes = (width as u64)
        .saturating_mul(height as u64)
        .saturating_mul(channels)
        .saturating_mul(bytes_per_sample);

    if decoded_bytes <= EAGER_VALIDATION_MAX_DECODED_BYTES {
        // Stream through (and discard) the rest of the rows to confirm the
        // whole image decodes without error.
        while reader.next_row().map_err(png_error_message)?.is_some() {}
    } else {
        drop(reader);
        validate_png_chunks(data)?;
    }

    Ok((width, height, has_alpha, icc, is_8bit))
}

/// The largest decoded size for which [`validate_png`] proves the pixel data
/// decodes by actually inflating all of it.
///
/// Above this, inflating the whole image up front is a full redundant
/// decompression pass -- the render path is about to decompress the same
/// bytes again, row range by row range -- which measurably dominates a large
/// poster's export time. Such files instead get [`validate_png_chunks`],
/// which is orders of magnitude cheaper and still catches the corruption
/// that actually happens to large assets in production: truncated uploads,
/// partial writes, and bit rot.
///
/// The residual risk is a file whose chunks are all intact but whose
/// compressed stream is nonetheless invalid -- which essentially requires a
/// deliberately crafted file. That surfaces as a panic from
/// [`RasterImage::decode_dynamic`] rather than a clean error, the same as
/// today's already-reachable case of the file changing underneath a
/// memory-mapped read.
const EAGER_VALIDATION_MAX_DECODED_BYTES: u64 = 32 * 1024 * 1024;

/// Walks a PNG's chunk structure and verifies every chunk's CRC, without
/// inflating any pixel data.
///
/// This reads the compressed bytes once, sequentially, releasing them as it
/// goes (see [`Bytes::drop_behind`]) so it doesn't leave a large asset
/// resident just for having been checked.
fn validate_png_chunks(data: &Bytes) -> StrResult<()> {
    /// Both errors mimic `png_error_message`'s formatting so that a
    /// malformed file reports the same way whichever validation ran.
    fn truncated() -> EcoString {
        EcoString::from(
            "failed to decode image (Format error decoding Png: file is truncated)",
        )
    }
    fn corrupt(kind: &[u8]) -> EcoString {
        let kind = String::from_utf8_lossy(kind);
        eco_format!(
            "failed to decode image (Format error decoding Png:              CRC error in chunk {kind})"
        )
    }

    let bytes = data.as_slice();
    let mut pos = 8; // The signature, already validated by `png_reader`.
    let mut released = 0;
    let mut saw_end = false;

    while pos < bytes.len() {
        let header = bytes.get(pos..pos + 8).ok_or_else(truncated)?;
        let length = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
        let kind = &header[4..8];

        // The CRC covers the chunk type and its data, but not the length.
        let body_end = pos
            .checked_add(8)
            .and_then(|start| start.checked_add(length))
            .ok_or_else(truncated)?;
        let crc_end = body_end.checked_add(4).ok_or_else(truncated)?;
        if crc_end > bytes.len() {
            return Err(truncated());
        }

        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&bytes[pos + 4..body_end]);
        let expected = u32::from_be_bytes(bytes[body_end..crc_end].try_into().unwrap());
        if hasher.finalize() != expected {
            return Err(corrupt(kind));
        }

        saw_end |= kind == b"IEND";
        pos = crc_end;
        if pos >= released + RELEASE_CHUNK {
            data.release(released..pos);
            released = pos;
        }
    }

    if !saw_end {
        return Err(truncated());
    }

    Ok(())
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
    fn test_rgba8_shared_across_concurrent_tiles() {
        let raster = RasterImage::new(
            Bytes::new(vec![42; 64 * 64]),
            RasterFormat::Pixel(PixelFormat {
                encoding: PixelEncoding::Luma8,
                width: 64,
                height: 64,
            }),
            Smart::Auto,
        )
        .unwrap();
        let barrier = std::sync::Barrier::new(4);
        let converted = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        raster.rgba8().clone()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect::<Vec<_>>()
        });
        for image in &converted {
            assert!(Arc::ptr_eq(image, &converted[0]));
            assert_eq!(image.as_rgba8().unwrap().get_pixel(0, 0).0, [42, 42, 42, 255]);
        }

        let native = RasterImage::new(
            Bytes::new(vec![42; 64 * 64 * 4]),
            RasterFormat::Pixel(PixelFormat {
                encoding: PixelEncoding::Rgba8,
                width: 64,
                height: 64,
            }),
            Smart::Auto,
        )
        .unwrap();
        assert!(Arc::ptr_eq(native.rgba8(), native.dynamic()));
    }

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

    /// A request that rewinds a little -- which is what the resampling
    /// render path does, since it overlaps consecutive bands by the resize
    /// filter's kernel support -- must be served from the cursor's retained
    /// tail and still match the full decode exactly.
    #[test]
    fn test_row_range_small_rewind_matches_full_decode() {
        #[track_caller]
        fn test(path: &str) {
            let data = typst_dev_assets::get(path).unwrap();
            let image =
                RasterImage::plain(Bytes::new(data), ExchangeFormat::Png).unwrap();
            let full = image.dynamic().to_rgba8();
            let (width, height) = (image.width(), image.height());
            let row = |y: u32| {
                let start = y as usize * width as usize * 4;
                &full.as_raw()[start..start + width as usize * 4]
            };

            // Walk forward in overlapping windows, the way banded rendering
            // does, then rewind to the very top again.
            let mut ranges = vec![];
            let mut y = 0;
            while y < height {
                ranges.push((y.saturating_sub(2), (y + 3).min(height)));
                y += 2;
            }
            ranges.push((0, height.min(3)));

            for (y0, y1) in ranges {
                let region = image.decode_rgba_row_range(y0, y1).unwrap();
                for y in y0..y1 {
                    let offset = (y - y0) as usize * width as usize * 4;
                    assert_eq!(
                        &region[offset..offset + width as usize * 4],
                        row(y),
                        "{path}: row {y} of range {y0}..{y1}"
                    );
                }
            }
        }

        test("images/chart-good.png");
        test("images/graph.png");
    }

    /// Reading an image in the overlapping windows the resampling render
    /// path uses must reuse one decoder throughout. Each extra start
    /// re-decompresses the image from row 0, which is what made a banded
    /// export cost `O(bands * height)` row decodes.
    #[test]
    fn test_overlapping_bands_reuse_one_decoder() {
        let height = 600;
        let data = encode_png8(8, height);
        let image = RasterImage::plain(Bytes::new(data), ExchangeFormat::Png).unwrap();

        // Mirrors `try_blit_resized_axis_aligned`: probe the channel count,
        // then request each band expanded by the filter's kernel support.
        let band = 64;
        let margin = 5;
        let mut y = 0;
        while y < height {
            assert!(image.supports_row_range(3));
            let y0 = y.saturating_sub(margin);
            let y1 = (y + band + margin).min(height);
            assert!(image.decode_row_range(y0, y1, 3).is_some());
            y += band;
        }

        assert_eq!(
            image.decoder_starts(),
            1,
            "overlapping row ranges must not restart the decoder"
        );
    }

    /// The row tiles of one band are rendered concurrently, so they reach
    /// the row cursor in an arbitrary order. With a window reserved to cover
    /// the band (see `RasterImage::reserve_retained_rows`) that must still
    /// cost exactly one decoder start, and each tile must get exactly the
    /// rows it asked for -- otherwise a tiled band would decode the image
    /// once per tile instead of once.
    #[test]
    fn test_out_of_order_tiles_reuse_one_decoder() {
        // A size no other test uses: `RasterImage::plain` is memoized on its
        // bytes, so sharing a fixture would share the decoder-start counter
        // this test asserts on.
        let (width, height) = (9_u32, 601_u32);
        let (band, tile, margin) = (120_u32, 30_u32, 5_u32);

        let image = RasterImage::plain(
            Bytes::new(encode_png8(width, height)),
            ExchangeFormat::Png,
        )
        .unwrap();
        image.reserve_retained_rows(band + 2 * margin);

        // `encode_png8` fills the image with `i % 251` in row-major order, so
        // the expected bytes for a row range are known without decoding
        // anything -- which keeps this independent of the cursor's state.
        let stride = (width * 3) as usize;
        let expected = |y0: u32, y1: u32| -> Vec<u8> {
            (y0 as usize * stride..y1 as usize * stride)
                .map(|i| (i % 251) as u8)
                .collect()
        };

        for band_y0 in (0..height).step_by(band as usize) {
            let band_y1 = (band_y0 + band).min(height);

            // Tiles arriving back to front: the worst possible order for a
            // cursor that can only move forwards.
            let tiles: Vec<u32> = (band_y0..band_y1).step_by(tile as usize).collect();
            for &ty in tiles.iter().rev() {
                let (y0, y1) =
                    (ty.saturating_sub(margin), (ty + tile + margin).min(height));
                assert_eq!(
                    image.decode_row_range(y0, y1, 3).unwrap(),
                    expected(y0, y1),
                    "rows {y0}..{y1} are wrong when tiles arrive out of order"
                );
            }
        }

        assert_eq!(
            image.decoder_starts(),
            1,
            "out-of-order tiles inside a reserved window must not restart \
             the decoder"
        );
    }

    /// A rewind further back than the retained tail has to restart the
    /// decoder, which must be transparent to the caller.
    #[test]
    fn test_row_range_long_rewind_matches_full_decode() {
        let height = RETAIN_MAX_ROWS as u32 * 3;
        let data = encode_png8(2, height);
        let image = RasterImage::plain(Bytes::new(data), ExchangeFormat::Png).unwrap();
        let full = image.dynamic().to_rgba8();
        let stride = image.width() as usize * 4;

        // Read to the end, so the tail no longer covers the first rows.
        let all = image.decode_rgba_row_range(0, height).unwrap();
        assert_eq!(all, full.as_raw()[..stride * height as usize]);

        // Now rewind past the tail; this restarts decompression from row 0.
        let region = image.decode_rgba_row_range(0, 2).unwrap();
        assert_eq!(region, &full.as_raw()[..stride * 2]);
    }

    /// Decoding to three channels must produce exactly the four-channel
    /// output with the (constant, opaque) alpha byte removed, and must
    /// refuse a source that has a real alpha channel.
    #[test]
    fn test_row_range_three_channels() {
        let data = typst_dev_assets::get("images/chart-good.png").unwrap();
        let rgb = RasterImage::plain(Bytes::new(data), ExchangeFormat::Png).unwrap();
        assert!(!rgb.has_alpha());
        assert!(rgb.supports_row_range(3));

        let height = rgb.height();
        let four = rgb.decode_row_range(0, height, 4).unwrap();
        let three = rgb.decode_row_range(0, height, 3).unwrap();
        assert_eq!(three.len(), four.len() / 4 * 3);
        for (rgba, rgb) in four.chunks_exact(4).zip(three.chunks_exact(3)) {
            assert_eq!(&rgba[..3], rgb);
            assert_eq!(rgba[3], 255);
        }

        let data = typst_dev_assets::get("images/graph.png").unwrap();
        let rgba = RasterImage::plain(Bytes::new(data), ExchangeFormat::Png).unwrap();
        assert!(rgba.has_alpha());
        assert!(!rgba.supports_row_range(3));
        assert!(rgba.decode_row_range(0, 1, 3).is_none());
    }

    /// Encodes a plain 8-bit RGB PNG of the given size.
    fn encode_png8(width: u32, height: u32) -> Vec<u8> {
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for i in 0..(width * height * 3) {
            data.push((i % 251) as u8);
        }

        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, width, height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&data).unwrap();
        }
        out
    }

    /// An image too large for eager validation is checked structurally
    /// instead (see `EAGER_VALIDATION_MAX_DECODED_BYTES`): a well-formed
    /// file must still load, and truncation or corruption must still be
    /// reported at load time rather than slipping through.
    #[test]
    fn test_large_png_structural_validation() {
        // Just past the eager-validation threshold, so `validate_png` takes
        // the chunk/CRC path.
        let width = 1024;
        let height = (EAGER_VALIDATION_MAX_DECODED_BYTES / (width as u64 * 3)) as u32 + 8;
        let data = encode_png8(width, height);

        let image =
            RasterImage::plain(Bytes::new(data.clone()), ExchangeFormat::Png).unwrap();
        assert_eq!((image.width(), image.height()), (width, height));

        // Truncated part-way through the pixel data.
        let truncated = data[..data.len() * 3 / 4].to_vec();
        let Err(err) = RasterImage::plain(Bytes::new(truncated), ExchangeFormat::Png)
        else {
            panic!("truncated file must not load");
        };
        assert!(err.contains("truncated"), "{err}");

        // A single flipped bit inside the compressed data.
        let mut corrupted = data.clone();
        let middle = corrupted.len() / 2;
        corrupted[middle] ^= 0xff;
        let Err(err) = RasterImage::plain(Bytes::new(corrupted), ExchangeFormat::Png)
        else {
            panic!("corrupted file must not load");
        };
        assert!(err.contains("CRC error"), "{err}");
    }

    /// Encodes a small in-memory PNG with varied, non-trivial 16-bit sample
    /// values (not just 0/max), to exercise `sample16_to_8`'s rounding
    /// across its range rather than only its endpoints.
    fn encode_png16(width: u32, height: u32, color_type: png::ColorType) -> Vec<u8> {
        let channels = match color_type {
            png::ColorType::Rgb => 3,
            png::ColorType::Rgba => 4,
            png::ColorType::Grayscale => 1,
            png::ColorType::GrayscaleAlpha => 2,
            png::ColorType::Indexed => unreachable!("test uses direct samples"),
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
            let expected =
                &full.as_raw()[(2 * width * 4) as usize..(4 * width * 4) as usize];
            assert_eq!(region, expected, "{color_type:?}: rows 2..4");
        }

        test(png::ColorType::Rgb);
        test(png::ColorType::Rgba);
        test(png::ColorType::Grayscale);
        test(png::ColorType::GrayscaleAlpha);
    }

    #[test]
    fn test_grayscale_row_ranges() {
        for depth in [
            png::BitDepth::One,
            png::BitDepth::Two,
            png::BitDepth::Four,
            png::BitDepth::Eight,
        ] {
            for alpha in [false, true] {
                // Low bit depths express transparency with tRNS.
                let explicit_alpha = alpha && depth == png::BitDepth::Eight;
                let channels = if explicit_alpha { 2 } else { 1 };
                let mut encoded = Vec::new();
                {
                    let mut encoder = png::Encoder::new(&mut encoded, 8, 5);
                    encoder.set_depth(depth);
                    encoder.set_color(if explicit_alpha {
                        png::ColorType::GrayscaleAlpha
                    } else {
                        png::ColorType::Grayscale
                    });
                    if alpha && !explicit_alpha {
                        encoder.set_trns(vec![0, 0]);
                    }
                    let mut writer = encoder.write_header().unwrap();
                    let len = 5 * depth as usize * channels;
                    let data: Vec<u8> = (0..len).map(|i| (i * 37) as u8).collect();
                    writer.write_image_data(&data).unwrap();
                }
                let image =
                    RasterImage::plain(Bytes::new(encoded), ExchangeFormat::Png).unwrap();
                assert!(image.supports_row_range(4));
                assert_eq!(image.supports_row_range(3), !alpha);
                // Out-of-order overlapping requests exercise retained raw rows.
                let ranges = [(2, 5), (1, 4), (0, 5)];
                let streamed: Vec<_> = ranges
                    .iter()
                    .map(|&(y0, y1)| image.decode_rgba_row_range(y0, y1).unwrap())
                    .collect();
                assert!(image.0.dynamic.get().is_none());
                let full = image.dynamic().to_rgba8();
                for ((y0, y1), rows) in ranges.into_iter().zip(streamed) {
                    assert_eq!(rows, full.as_raw()[y0 as usize * 32..y1 as usize * 32]);
                }
                if alpha {
                    assert!(image.decode_row_range(0, 5, 3).is_none());
                } else {
                    assert_eq!(
                        image.decode_row_range(0, 5, 3).unwrap(),
                        image.dynamic().to_rgb8().into_raw()
                    );
                }
            }
        }
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

        // Grayscale PNGs now stream as well.
        let gray = typst_dev_assets::get("screenshots/3-advanced-paper.png").unwrap();
        let gray = RasterImage::plain(Bytes::new(gray), ExchangeFormat::Png).unwrap();
        assert_eq!(
            gray.decode_rgba_row_range(0, 1).unwrap(),
            gray.dynamic().to_rgba8().as_raw()[..gray.width() as usize * 4]
        );

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
