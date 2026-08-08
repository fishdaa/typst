use fast_image_resize::images::Image as FirImage;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use hayro::RenderCache;
use hayro::RenderSettings;
use hayro::hayro_interpret::InterpreterSettings;
use hayro::hayro_interpret::font::{FontData, FontQuery, StandardFont};
use hayro::vello_cpu::color::palette::css::TRANSPARENT;
use image::{GenericImageView, Rgba};
use std::sync::Arc;
use tiny_skia as sk;
use tiny_skia::IntSize;
use typst_library::foundations::Smart;
use typst_library::layout::Size;
use typst_library::visualize::{Image, ImageKind, ImageScaling, PdfImage};

use crate::{AbsExt, State};

/// Render a raster or SVG image into the canvas.
pub fn render_image(
    canvas: &mut sk::Pixmap,
    state: State,
    image: &Image,
    size: Size,
) -> Option<()> {
    let ts = state.transform;
    let view_width = size.x.to_f32();
    let view_height = size.y.to_f32();

    if try_blit_opaque(canvas, &state, image, view_width, view_height).is_some() {
        return Some(());
    }

    // For better-looking output, resize `image` to its final size before
    // painting it to `canvas`. For the math, see:
    // https://github.com/typst/typst/issues/1404#issuecomment-1598374652
    let theta = libm::atan2f(-ts.kx, ts.sx);

    // To avoid division by 0, choose the one of { sin, cos } that is
    // further from 0.
    let prefer_sin = libm::sinf(theta).abs() > std::f32::consts::FRAC_1_SQRT_2;
    let scale_x = f32::abs(if prefer_sin {
        ts.kx / libm::sinf(theta)
    } else {
        ts.sx / libm::cosf(theta)
    });

    let aspect = (image.width() as f32) / (image.height() as f32);
    let w = (scale_x * view_width.max(aspect * view_height)).ceil() as u32;
    let h = ((w as f32) / aspect).ceil() as u32;

    let pixmap = build_texture(image, w, h)?;
    let paint_scale_x = view_width / pixmap.width() as f32;
    let paint_scale_y = view_height / pixmap.height() as f32;

    let paint = sk::Paint {
        shader: sk::Pattern::new(
            (*pixmap).as_ref(),
            sk::SpreadMode::Pad,
            sk::FilterQuality::Nearest,
            1.0,
            sk::Transform::from_scale(paint_scale_x, paint_scale_y),
        ),
        ..Default::default()
    };

    let rect = sk::Rect::from_xywh(0.0, 0.0, view_width, view_height)?;
    canvas.fill_rect(rect, &paint, ts, state.mask);

    Some(())
}

/// Fast path for a common but expensive case: a fully opaque raster image
/// painted at its native resolution with no rotation, skew, flip, or mask
/// (e.g. a full-bleed background on a large page/poster).
///
/// The general path always builds a full extra `sk::Pixmap` texture (RGBA8,
/// same pixel count as the source) purely to hand off to tiny-skia's
/// `Pattern` shader, then does a second full compositing pass over the
/// canvas. When the image is opaque, `src over dst` with `srcAlpha == 1`
/// equals `src` regardless of `dst`, so we can skip both: convert and write
/// each source pixel directly into its final canvas position. For a large
/// image this roughly halves peak memory (no texture buffer) and skips a
/// redundant full-canvas blend pass.
///
/// Returns `None` (with no side effects) whenever a precondition doesn't
/// hold, so callers should fall back to the general path unchanged.
fn try_blit_opaque(
    canvas: &mut sk::Pixmap,
    state: &State,
    image: &Image,
    view_width: f32,
    view_height: f32,
) -> Option<()> {
    // A mask (e.g. from a clip) requires per-pixel blending against existing
    // content, which this path doesn't do.
    if state.mask.is_some() {
        return None;
    }

    // Only handle axis-aligned scale + translate: no rotation, skew, or flip.
    let ts = state.transform;
    if ts.kx != 0.0 || ts.ky != 0.0 || ts.sx <= 0.0 || ts.sy <= 0.0 {
        return None;
    }

    let ImageKind::Raster(raster) = image.kind() else { return None };
    let dynamic = raster.dynamic();

    // Without an alpha channel, every pixel is fully opaque, so overwriting
    // is always correct regardless of what's beneath.
    if dynamic.color().has_alpha() {
        return None;
    }

    let src_w = dynamic.width();
    let src_h = dynamic.height();

    // Compute the destination pixel rect and require it to land exactly on
    // the pixel grid at the image's native resolution (i.e. no resampling
    // would be needed by the general path either).
    let x0 = ts.tx;
    let y0 = ts.ty;
    let x1 = ts.sx * view_width + ts.tx;
    let y1 = ts.sy * view_height + ts.ty;

    const EPS: f32 = 0.01;
    let (rx0, ry0, rx1, ry1) = (x0.round(), y0.round(), x1.round(), y1.round());
    if (x0 - rx0).abs() > EPS
        || (y0 - ry0).abs() > EPS
        || (x1 - rx1).abs() > EPS
        || (y1 - ry1).abs() > EPS
    {
        return None;
    }

    let dst_w = (rx1 - rx0) as i64;
    let dst_h = (ry1 - ry0) as i64;
    if dst_w != src_w as i64 || dst_h != src_h as i64 {
        return None;
    }

    // The destination rect may extend beyond the canvas: callers rendering a
    // large page in horizontal bands (see `render_band`) pass a canvas that
    // only covers one band, so a full-page background image only partially
    // overlaps it. Clip to the overlap and blit just that portion, rather
    // than requiring (and, on the general path, allocating a texture for)
    // the whole image.
    let (dst_x0, dst_y0) = (rx0 as i64, ry0 as i64);
    let clip_x0 = dst_x0.max(0);
    let clip_y0 = dst_y0.max(0);
    let clip_x1 = (dst_x0 + dst_w).min(canvas.width() as i64);
    let clip_y1 = (dst_y0 + dst_h).min(canvas.height() as i64);
    if clip_x0 >= clip_x1 || clip_y0 >= clip_y1 {
        // No overlap with the canvas at all (e.g. a band that this image
        // doesn't touch).
        return None;
    }

    let canvas_w = canvas.width() as usize;
    let pixels = canvas.pixels_mut();

    // Source-space row/column range that maps into the clipped destination
    // rect (still a 1:1 mapping, since we required native resolution above).
    let src_y_range = (clip_y0 - dst_y0) as u32..(clip_y1 - dst_y0) as u32;
    let src_x_range = (clip_x0 - dst_x0) as u32..(clip_x1 - dst_x0) as u32;
    for sy in src_y_range {
        let py = (dst_y0 + sy as i64) as usize;
        for sx in src_x_range.clone() {
            let px = (dst_x0 + sx as i64) as usize;
            let Rgba([r, g, b, _]) = dynamic.get_pixel(sx, sy);
            pixels[py * canvas_w + px] = sk::ColorU8::from_rgba(r, g, b, 255).premultiply();
        }
    }

    Some(())
}

/// Prepare a texture for an image at a scaled size.
#[comemo::memoize]
fn build_texture(image: &Image, w: u32, h: u32) -> Option<Arc<sk::Pixmap>> {
    let texture = match image.kind() {
        ImageKind::Raster(raster) => {
            let mut texture = sk::Pixmap::new(w, h)?;
            let w = texture.width();
            let h = texture.height();

            let dynamic = raster.dynamic();
            if (w, h) == (dynamic.width(), dynamic.height()) {
                // Small optimization to not allocate in case image is not resized.
                for ((_, _, Rgba([r, g, b, a])), dest) in
                    dynamic.pixels().zip(texture.pixels_mut())
                {
                    *dest = sk::ColorU8::from_rgba(r, g, b, a).premultiply();
                }
            } else {
                let upscale = w > dynamic.width();
                let filter = match image.scaling() {
                    Smart::Custom(ImageScaling::Pixelated) => None,
                    _ if upscale => Some(FilterType::CatmullRom),
                    _ => Some(FilterType::Lanczos3), // downscale
                };
                let alg = match filter {
                    Some(filter) => ResizeAlg::Convolution(filter),
                    None => ResizeAlg::Nearest,
                };

                // Resizing (rather than the final premultiply pass below) is
                // the expensive part for a large placed image (e.g. a
                // full-bleed poster/certificate background), so this uses
                // `fast_image_resize`, which is SIMD-accelerated and (via its
                // `rayon` feature) parallelizes the convolution across
                // threads, instead of `image`'s single-threaded scalar
                // resize.
                let src = dynamic.to_rgba8();
                let mut dst = FirImage::new(w, h, PixelType::U8x4);
                Resizer::new()
                    .resize(&src, &mut dst, &ResizeOptions::new().resize_alg(alg))
                    .ok()?;

                let chunks = dst.buffer().chunks_exact(4);
                for (src, dest) in chunks.zip(texture.pixels_mut()) {
                    *dest = sk::ColorU8::from_rgba(src[0], src[1], src[2], src[3])
                        .premultiply();
                }
            }

            texture
        }
        ImageKind::Svg(svg) => {
            let mut texture = sk::Pixmap::new(w, h)?;
            let tree = svg.tree();
            let ts = tiny_skia::Transform::from_scale(
                w as f32 / tree.size().width(),
                h as f32 / tree.size().height(),
            );
            resvg::render(tree, ts, &mut texture.as_mut());
            texture
        }
        ImageKind::Pdf(pdf) => build_pdf_texture(pdf, w, h)?,
    };

    Some(Arc::new(texture))
}

// Keep this in sync with `typst-svg`!
fn build_pdf_texture(pdf: &PdfImage, w: u32, h: u32) -> Option<sk::Pixmap> {
    let select_standard_font = move |font: StandardFont| -> Option<(FontData, u32)> {
        let bytes = match font {
            StandardFont::Helvetica => typst_assets::pdf::SANS,
            StandardFont::HelveticaBold => typst_assets::pdf::SANS_BOLD,
            StandardFont::HelveticaOblique => typst_assets::pdf::SANS_ITALIC,
            StandardFont::HelveticaBoldOblique => typst_assets::pdf::SANS_BOLD_ITALIC,
            StandardFont::Courier => typst_assets::pdf::FIXED,
            StandardFont::CourierBold => typst_assets::pdf::FIXED_BOLD,
            StandardFont::CourierOblique => typst_assets::pdf::FIXED_ITALIC,
            StandardFont::CourierBoldOblique => typst_assets::pdf::FIXED_BOLD_ITALIC,
            StandardFont::TimesRoman => typst_assets::pdf::SERIF,
            StandardFont::TimesBold => typst_assets::pdf::SERIF_BOLD,
            StandardFont::TimesItalic => typst_assets::pdf::SERIF_ITALIC,
            StandardFont::TimesBoldItalic => typst_assets::pdf::SERIF_BOLD_ITALIC,
            StandardFont::ZapfDingBats => typst_assets::pdf::DING_BATS,
            StandardFont::Symbol => typst_assets::pdf::SYMBOL,
        };
        Some((Arc::new(bytes), 0))
    };

    let interpreter_settings = InterpreterSettings {
        font_resolver: Arc::new(move |query| match query {
            FontQuery::Standard(s) => select_standard_font(*s),
            FontQuery::Fallback(f) => select_standard_font(f.pick_standard_font()),
        }),
        // Fairly niche and enabling hayro's embedded cmap would add a
        // considerable amount of data.
        cmap_resolver: Arc::new(|_| None),
        warning_sink: Arc::new(|_| {}),
        // We want to render like it prints, so no annotations.
        render_annotations: false,
    };

    let render_settings = RenderSettings {
        x_scale: w as f32 / pdf.width(),
        y_scale: h as f32 / pdf.height(),
        width: Some(w as u16),
        height: Some(h as u16),
        bg_color: TRANSPARENT,
    };

    let cache = RenderCache::new();
    let hayro_pix =
        hayro::render(pdf.page(), &cache, &interpreter_settings, &render_settings);

    let bytes: Vec<u8> = bytemuck::cast_vec(hayro_pix.take());
    sk::Pixmap::from_vec(bytes, IntSize::from_wh(w, h)?)
}
