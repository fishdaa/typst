use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};

use ecow::eco_format;
use image::{DynamicImage, EncodableLayout, GenericImageView, Rgba};
use krilla::image::{BitsPerComponent, CustomImage, ImageColorspace};
use krilla::pdf::PdfDocument;
use krilla::surface::Surface;
use krilla_svg::{SurfaceExt, SvgSettings};
use typst_library::diag::{At, SourceResult};
use typst_library::foundations::Smart;
use typst_library::layout::{Abs, Angle, Ratio, Size, Transform};
use typst_library::visualize::{
    ExchangeFormat, Image, ImageKind, ImageScaling, PdfImage, RasterFormat, RasterImage,
};
use typst_syntax::Span;
use typst_utils::defer;

use crate::convert::{FrameContext, GlobalContext};
use crate::tags;
use crate::util::{SizeExt, TransformExt};

#[typst_macros::time(name = "handle image")]
pub(crate) fn handle_image(
    gc: &mut GlobalContext,
    fc: &mut FrameContext,
    image: &Image,
    size: Size,
    surface: &mut Surface,
    span: Span,
) -> SourceResult<()> {
    surface.push_transform(&fc.state().transform().to_krilla());
    surface.set_location(span.into_raw());
    let mut surface = defer(surface, |s| {
        s.pop();
        s.reset_location();
    });

    let interpolate = image.scaling() == Smart::Custom(ImageScaling::Smooth);

    gc.image_spans.insert(span);

    let mut handle = tags::image(gc, fc, &mut surface, image, size);
    let surface = handle.surface();

    match image.kind() {
        ImageKind::Raster(raster) => {
            let (exif_transform, new_size) = exif_transform(raster, size);
            surface.push_transform(&exif_transform.to_krilla());
            let mut surface = defer(surface, |s| s.pop());

            let image = convert_raster(raster.clone(), interpolate)
                .map_err(|err| eco_format!("failed to process image ({err})"))
                .at(span)?;

            if !gc.image_to_spans.contains_key(&image) {
                gc.image_to_spans.insert(image.clone(), span);
            }

            if let Some(size) = new_size.to_krilla() {
                surface.draw_image(image, size);
            }
        }
        ImageKind::Svg(svg) => {
            if let Some(size) = size.to_krilla() {
                surface.draw_svg(
                    svg.tree(),
                    size,
                    SvgSettings { embed_text: true, ..Default::default() },
                );
            }
        }
        ImageKind::Pdf(pdf) => {
            if let Some(size) = size.to_krilla() {
                surface.draw_pdf_page(&convert_pdf(pdf), size, pdf.page_index());
            }
        }
    }

    Ok(())
}

/// A wrapper around `RasterImage` so that we can implement `CustomImage`.
#[derive(Clone)]
struct PdfRasterImage(Arc<PdfRasterImageInner>);

/// The internal representation of a [`PdfRasterImage`].
struct PdfRasterImageInner {
    /// The original, underlying raster image.
    raster: RasterImage,
    /// The color and alpha channels split out of `raster`, computed together
    /// in a single pass (see [`derive_channels`]).
    channels: OnceLock<Channels>,
}

impl PdfRasterImage {
    /// Wraps a raster image.
    pub fn new(raster: RasterImage) -> Self {
        Self(Arc::new(PdfRasterImageInner { raster, channels: OnceLock::new() }))
    }
}

impl Hash for PdfRasterImage {
    fn hash<H: Hasher>(&self, state: &mut H) {
        // `channels` is generated from the underlying `RasterImage`, so this
        // is enough. Since `raster` is prehashed, this is also very cheap.
        self.0.raster.hash(state);
    }
}

/// A color buffer for [`Channels`], avoiding a copy for the common case
/// where `raster.dynamic()` is already exactly the right pixel type (a plain
/// refcount bump via `Arc::clone`).
enum ColorBuf {
    Dynamic(Arc<DynamicImage>),
    Owned(Vec<u8>),
}

impl ColorBuf {
    fn as_bytes(&self) -> &[u8] {
        match self {
            ColorBuf::Dynamic(dynamic) => dynamic.as_bytes(),
            ColorBuf::Owned(buf) => buf,
        }
    }
}

/// The color and alpha channels needed to embed a [`RasterImage`] in a PDF.
struct Channels {
    color: ColorBuf,
    is_rgb: bool,
    alpha: Option<Vec<u8>>,
    /// Whether `raster`'s ICC profile (if any) still applies to `color`, or
    /// was invalidated by a lossy bit-depth reduction while deriving it.
    icc_valid: bool,
}

/// Splits a raster image into a color channel (guaranteed luma8 or rgb8, for
/// `CustomImage::color_channel`) and an optional alpha channel, in a single
/// pass over the source pixels where possible.
///
/// For a qualifying PNG (see [`RasterImage::for_each_rgba_row`]), this
/// decodes directly via the row-streaming path instead of
/// [`RasterImage::dynamic`], so no whole-image interleaved RGBA buffer is
/// created while `color`/`alpha` are built. Otherwise (non-PNG, interlaced,
/// or EXIF-rotated images), this falls back to `raster.dynamic()`.
///
/// Splitting used to happen as two independent passes -- `color_channel`
/// building its converted buffer via `to_rgb8()`/`to_luma8()`, and
/// `alpha_channel` separately re-walking every pixel via the `pixels()`
/// iterator, which visited the source buffer twice and paid for
/// `pixels()`'s per-pixel bounds-checked indexing. Both are now produced in
/// one direct pass over the raw byte slice instead, for the same result.
fn derive_channels(raster: &RasterImage) -> Channels {
    if raster.exif_rotation().is_none()
        && matches!(raster.format(), RasterFormat::Exchange(ExchangeFormat::Png))
    {
        let pixels = raster.width() as usize * raster.height() as usize;
        let mut color = Vec::with_capacity(pixels * 3);
        let mut alpha = raster.has_alpha().then(|| Vec::with_capacity(pixels));
        if raster
            .for_each_rgba_row(0, raster.height(), |_, row| {
                for px in row.chunks_exact(4) {
                    color.extend_from_slice(&px[..3]);
                    if let Some(alpha) = &mut alpha {
                        alpha.push(px[3]);
                    }
                }
            })
            .is_some()
        {
            return Channels {
                color: ColorBuf::Owned(color),
                is_rgb: true,
                alpha,
                icc_valid: raster.is_native_8bit(),
            };
        }
    }

    let dynamic = raster.dynamic();
    match dynamic.as_ref() {
        // Pure luma8 or rgb8 image: no alpha, use it directly.
        DynamicImage::ImageLuma8(_) => Channels {
            color: ColorBuf::Dynamic(dynamic.clone()),
            is_rgb: false,
            alpha: None,
            icc_valid: true,
        },
        DynamicImage::ImageRgb8(_) => Channels {
            color: ColorBuf::Dynamic(dynamic.clone()),
            is_rgb: true,
            alpha: None,
            icc_valid: true,
        },
        // Rgba8: split into rgb8 + alpha in one pass.
        DynamicImage::ImageRgba8(buf) => {
            let pixels = buf.width() as usize * buf.height() as usize;
            let mut rgb = Vec::with_capacity(pixels * 3);
            let mut alpha = Vec::with_capacity(pixels);
            for px in buf.as_raw().chunks_exact(4) {
                rgb.extend_from_slice(&px[..3]);
                alpha.push(px[3]);
            }
            Channels {
                color: ColorBuf::Owned(rgb),
                is_rgb: true,
                alpha: Some(alpha),
                icc_valid: true,
            }
        }
        // LumaA8: split into luma8 + alpha in one pass.
        DynamicImage::ImageLumaA8(buf) => {
            let pixels = buf.width() as usize * buf.height() as usize;
            let mut luma = Vec::with_capacity(pixels);
            let mut alpha = Vec::with_capacity(pixels);
            for px in buf.as_raw().chunks_exact(2) {
                luma.push(px[0]);
                alpha.push(px[1]);
            }
            Channels {
                color: ColorBuf::Owned(luma),
                is_rgb: false,
                alpha: Some(alpha),
                icc_valid: true,
            }
        }
        // Anything else (e.g. 16-bit-per-channel): fall back to `image`'s
        // general conversion, which handles bit-depth downsampling. The ICC
        // profile (if any) is invalidated by that conversion.
        _ => {
            let channel_count = dynamic.color().channel_count();
            let is_rgb = channel_count > 2;
            let color = if is_rgb {
                ColorBuf::Owned(dynamic.to_rgb8().into_raw())
            } else {
                ColorBuf::Owned(dynamic.to_luma8().into_raw())
            };
            let alpha = dynamic.color().has_alpha().then(|| {
                dynamic.pixels().map(|(_, _, Rgba([_, _, _, a]))| a).collect()
            });
            Channels { color, is_rgb, alpha, icc_valid: false }
        }
    }
}

impl CustomImage for PdfRasterImage {
    fn color_channel(&self) -> &[u8] {
        self.0
            .channels
            .get_or_init(|| derive_channels(&self.0.raster))
            .color
            .as_bytes()
    }

    fn alpha_channel(&self) -> Option<&[u8]> {
        self.0
            .channels
            .get_or_init(|| derive_channels(&self.0.raster))
            .alpha
            .as_deref()
    }

    fn bits_per_component(&self) -> BitsPerComponent {
        BitsPerComponent::Eight
    }

    fn size(&self) -> (u32, u32) {
        (self.0.raster.width(), self.0.raster.height())
    }

    fn icc_profile(&self) -> Option<&[u8]> {
        let channels = self.0.channels.get_or_init(|| derive_channels(&self.0.raster));
        channels.icc_valid.then(|| self.0.raster.icc().map(|b| b.as_bytes())).flatten()
    }

    fn color_space(&self) -> ImageColorspace {
        let channels = self.0.channels.get_or_init(|| derive_channels(&self.0.raster));
        if channels.is_rgb { ImageColorspace::Rgb } else { ImageColorspace::Luma }
    }
}

#[comemo::memoize]
fn convert_raster(
    raster: RasterImage,
    interpolate: bool,
) -> Result<krilla::image::Image, String> {
    if let RasterFormat::Exchange(ExchangeFormat::Jpg) = raster.format() {
        let image_data: Arc<dyn AsRef<[u8]> + Send + Sync> =
            Arc::new(raster.data().clone());
        let icc_profile = raster.icc().map(|i| {
            let i: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(i.clone());
            i
        });

        krilla::image::Image::from_jpeg_with_icc(
            image_data.into(),
            icc_profile.map(|i| i.into()),
            interpolate,
        )
    } else if matches!(raster.format(), RasterFormat::Exchange(ExchangeFormat::Png))
        && raster.exif_rotation().is_none()
        && raster.icc().is_none()
    {
        // Keep ordinary PNGs compressed and deferred all the way through PDF
        // serialization. The custom-image path below decodes the whole image
        // and retains separate color/alpha buffers, which is unnecessarily
        // expensive for a PNG that krilla can embed directly.
        let image_data: Arc<dyn AsRef<[u8]> + Send + Sync> = Arc::new(raster.data().clone());
        krilla::image::Image::from_png(image_data.into(), interpolate)
    } else {
        krilla::image::Image::from_custom(PdfRasterImage::new(raster), interpolate)
    }
}

#[comemo::memoize]
fn convert_pdf(pdf: &PdfImage) -> PdfDocument {
    PdfDocument::new(pdf.document().pdf().clone())
}

fn exif_transform(image: &RasterImage, size: Size) -> (Transform, Size) {
    // For JPEGs, we want to apply the EXIF orientation as a transformation
    // because we don't recode them. For other formats, the transform is already
    // baked into the dynamic image data.
    if image.format() != RasterFormat::Exchange(ExchangeFormat::Jpg) {
        return (Transform::identity(), size);
    }

    let base = |hp: bool, vp: bool, mut base_ts: Transform, size: Size| {
        if hp {
            // Flip horizontally in-place.
            base_ts = base_ts.pre_concat(
                Transform::scale(-Ratio::one(), Ratio::one())
                    .pre_concat(Transform::translate(-size.x, Abs::zero())),
            );
        }

        if vp {
            // Flip vertically in-place.
            base_ts = base_ts.pre_concat(
                Transform::scale(Ratio::one(), -Ratio::one())
                    .pre_concat(Transform::translate(Abs::zero(), -size.y)),
            );
        }

        base_ts
    };

    let no_flipping =
        |hp: bool, vp: bool| (base(hp, vp, Transform::identity(), size), size);

    let with_flipping = |hp: bool, vp: bool| {
        let base_ts = Transform::rotate_at(Angle::deg(90.0), Abs::zero(), Abs::zero())
            .pre_concat(Transform::scale(Ratio::one(), -Ratio::one()));
        let inv_size = Size::new(size.y, size.x);
        (base(hp, vp, base_ts, inv_size), inv_size)
    };

    match image.exif_rotation() {
        Some(2) => no_flipping(true, false),
        Some(3) => no_flipping(true, true),
        Some(4) => no_flipping(false, true),
        Some(5) => with_flipping(false, false),
        Some(6) => with_flipping(false, true),
        Some(7) => with_flipping(true, true),
        Some(8) => with_flipping(true, false),
        _ => no_flipping(false, false),
    }
}
