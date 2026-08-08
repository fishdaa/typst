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

    if try_blit_resized_axis_aligned(canvas, &state, image, view_width, view_height)
        .is_some()
    {
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

/// Fast path for a raster image that needs resampling (i.e. doesn't qualify
/// for `try_blit_opaque`) but is placed axis-aligned and pixel-grid-aligned.
///
/// The general path (`build_texture`) resamples the *entire* placed image
/// into a texture sized to its full destination extent, even when the
/// visible canvas only covers a fraction of it -- e.g. one horizontal band
/// of a large poster (see `render_band`). For a large upscaled background,
/// that full-extent texture (and the resize buffer used to build it) can be
/// hundreds of megabytes or more, held for the whole export even though any
/// single band only needs a sliver of it.
///
/// This instead resamples only the source region that maps to the visible
/// canvas, plus a small margin for the resampling filter's kernel support,
/// using `fast_image_resize`'s source cropping to crop before resizing. The
/// crop is constructed so its implied scale factor exactly matches the
/// unclipped resize's scale factor, keeping both on the same sampling grid;
/// residual differences from resizing in pieces rather than all at once are
/// sub-pixel floating-point rounding at tile seams (a couple of levels out
/// of 255), not a change in method.
///
/// Returns `None` (with no side effects) whenever a precondition doesn't
/// hold, so callers should fall back to the general path unchanged.
fn try_blit_resized_axis_aligned(
    canvas: &mut sk::Pixmap,
    state: &State,
    image: &Image,
    view_width: f32,
    view_height: f32,
) -> Option<()> {
    // Only handle axis-aligned scale + translate: no rotation, skew, or flip.
    let ts = state.transform;
    if ts.kx != 0.0 || ts.ky != 0.0 || ts.sx <= 0.0 || ts.sy <= 0.0 {
        return None;
    }

    let ImageKind::Raster(raster) = image.kind() else { return None };
    let dynamic = raster.dynamic();
    let (src_w, src_h) = (dynamic.width(), dynamic.height());

    // Compute the destination pixel rect and require it to land exactly on
    // the pixel grid, so tiles can be copied into the canvas without needing
    // edge antialiasing.
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

    let dst_x0 = rx0 as i64;
    let dst_y0 = ry0 as i64;
    let (dst_w, dst_h) = (rx1 - rx0, ry1 - ry0);
    if dst_w <= 0.0 || dst_h <= 0.0 || (dst_w, dst_h) == (src_w as f32, src_h as f32) {
        // Either degenerate, or exactly native resolution -- the latter is
        // `try_blit_opaque`'s job when opaque, and otherwise cheap enough
        // (a straight copy, no resampling) for the general path to handle.
        return None;
    }
    let (dst_w, dst_h) = (dst_w as u32, dst_h as u32);

    // The destination rect may extend beyond the canvas: callers rendering a
    // large page in horizontal bands (see `render_band`) pass a canvas that
    // only covers one band, so a full-page background image only partially
    // overlaps it.
    let clip_x0 = dst_x0.max(0);
    let clip_y0 = dst_y0.max(0);
    let clip_x1 = (dst_x0 + dst_w as i64).min(canvas.width() as i64);
    let clip_y1 = (dst_y0 + dst_h as i64).min(canvas.height() as i64);
    if clip_x0 >= clip_x1 || clip_y0 >= clip_y1 {
        // No overlap with this band/canvas at all.
        return Some(());
    }

    let scale_x = src_w as f64 / dst_w as f64;
    let scale_y = src_h as f64 / dst_h as f64;
    let upscale = dst_w > src_w || dst_h > src_h;
    let (alg, support) = match image.scaling() {
        Smart::Custom(ImageScaling::Pixelated) => (ResizeAlg::Nearest, 0.0),
        _ if upscale => (ResizeAlg::Convolution(FilterType::CatmullRom), 2.0),
        _ => (ResizeAlg::Convolution(FilterType::Lanczos3), 3.0),
    };

    // Expand the visible tile by the filter's kernel support (plus a little
    // slack) on each side, in destination pixels, then crop the source to
    // the matching region -- so pixels near the tile's edges are resampled
    // from the same neighborhood an unclipped resize would have used.
    let margin_x = (support * scale_x.max(1.0)).ceil() as i64 + 2;
    let margin_y = (support * scale_y.max(1.0)).ceil() as i64 + 2;

    let local_x0 = clip_x0 - dst_x0;
    let local_y0 = clip_y0 - dst_y0;
    let local_x1 = clip_x1 - dst_x0;
    let local_y1 = clip_y1 - dst_y0;

    let start_x = (local_x0 - margin_x).clamp(0, dst_w as i64) as u32;
    let start_y = (local_y0 - margin_y).clamp(0, dst_h as i64) as u32;
    let end_x = (local_x1 + margin_x).clamp(0, dst_w as i64) as u32;
    let end_y = (local_y1 + margin_y).clamp(0, dst_h as i64) as u32;
    let (crop_w, crop_h) = (end_x - start_x, end_y - start_y);
    if crop_w == 0 || crop_h == 0 {
        return Some(());
    }

    // Constructed so the crop's own implied scale factor is exactly
    // `scale_x`/`scale_y` -- i.e. identical to the unclipped resize's --
    // rather than picking a source crop and rounding the destination size
    // independently, which would drift the sampling grid across tiles.
    let crop_left = start_x as f64 * scale_x;
    let crop_top = start_y as f64 * scale_y;
    let crop_width = crop_w as f64 * scale_x;
    let crop_height = crop_h as f64 * scale_y;

    let src = to_rgba8(image)?;
    let mut resized = FirImage::new(crop_w, crop_h, PixelType::U8x4);
    let opts = ResizeOptions::new()
        .resize_alg(alg)
        .crop(crop_left, crop_top, crop_width, crop_height);
    Resizer::new().resize(src.as_ref(), &mut resized, &opts).ok()?;

    let (tile_w, tile_h) = ((clip_x1 - clip_x0) as u32, (clip_y1 - clip_y0) as u32);
    let mut tile = sk::Pixmap::new(tile_w, tile_h)?;
    let offset_x = (local_x0 as u32) - start_x;
    let offset_y = (local_y0 as u32) - start_y;
    let buf = resized.buffer();
    for row in 0..tile_h {
        let row_start = (((offset_y + row) * crop_w + offset_x) * 4) as usize;
        let row_bytes = &buf[row_start..row_start + (tile_w as usize) * 4];
        let dest_start = (row * tile_w) as usize;
        let dest_row = &mut tile.pixels_mut()[dest_start..dest_start + tile_w as usize];
        for (chunk, dest) in row_bytes.chunks_exact(4).zip(dest_row) {
            *dest = sk::ColorU8::from_rgba(chunk[0], chunk[1], chunk[2], chunk[3])
                .premultiply();
        }
    }

    canvas.draw_pixmap(
        clip_x0 as i32,
        clip_y0 as i32,
        tile.as_ref(),
        &sk::PixmapPaint::default(),
        sk::Transform::identity(),
        state.mask,
    );

    Some(())
}

/// Converts a raster image to RGBA8, memoized so repeated calls (e.g. once
/// per rendered band of a large page) reuse the same buffer instead of
/// redecoding/reconverting the whole source image each time.
#[comemo::memoize]
fn to_rgba8(image: &Image) -> Option<Arc<image::RgbaImage>> {
    let ImageKind::Raster(raster) = image.kind() else { return None };
    Some(Arc::new(raster.dynamic().to_rgba8()))
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
                let src = to_rgba8(image)?;
                let mut dst = FirImage::new(w, h, PixelType::U8x4);
                Resizer::new()
                    .resize(src.as_ref(), &mut dst, &ResizeOptions::new().resize_alg(alg))
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
