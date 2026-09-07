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
use typst_library::visualize::{Image, ImageKind, ImageScaling, PdfImage, RasterImage};

use crate::{AbsExt, State};

/// Render a raster or SVG image into the canvas.
pub fn render_image(
    canvas: &mut sk::PixmapMut,
    state: State,
    image: &Image,
    size: Size,
) -> Option<()> {
    let ts = state.transform;
    let view_width = size.x.to_f32();
    let view_height = size.y.to_f32();

    if try_render_svg(canvas, &state, image, view_width, view_height).is_some() {
        return Some(());
    }

    if try_blit_opaque(canvas, &state, image, view_width, view_height).is_some() {
        return Some(());
    }

    if try_blit_native_alpha(canvas, &state, image, view_width, view_height).is_some() {
        return Some(());
    }

    if try_blit_resized_axis_aligned(canvas, &state, image, view_width, view_height)
        .is_some()
    {
        return Some(());
    }

    if try_blit_resized_general(canvas, &state, image, view_width, view_height).is_some()
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

/// Render an SVG directly into the destination canvas instead of first
/// rasterizing the entire placed image into a texture. The canvas may be only
/// one render band, so resvg clips the result to the visible portion and keeps
/// memory proportional to the output band rather than the full SVG size.
fn try_render_svg(
    canvas: &mut sk::PixmapMut,
    state: &State,
    image: &Image,
    view_width: f32,
    view_height: f32,
) -> Option<()> {
    let ImageKind::Svg(svg) = image.kind() else { return None };

    // Keep the established texture path for ordinary SVGs. Direct resvg
    // rendering is intentionally reserved for placements large enough that
    // the old full-image texture becomes a material memory problem: the two
    // paths have small rasterization differences, and preserving the old path
    // for normal-sized assets keeps existing render output stable.
    const DIRECT_RENDER_THRESHOLD: u64 = 16 * 1024 * 1024;
    let pixels = (view_width.max(0.0) as u64).saturating_mul(view_height.max(0.0) as u64);
    if pixels < DIRECT_RENDER_THRESHOLD {
        return None;
    }

    let scale = sk::Transform::from_scale(
        view_width / svg.width() as f32,
        view_height / svg.height() as f32,
    );
    let transform = state.transform.pre_concat(scale);

    if let Some(mask) = state.mask {
        // Render into a canvas-sized temporary so the mask can be applied by
        // tiny-skia during compositing. This remains bounded by the current
        // render band instead of the full placed SVG dimensions.
        let mut rendered = sk::Pixmap::new(canvas.width(), canvas.height())?;
        resvg::render(svg.tree(), transform, &mut rendered.as_mut());
        canvas.draw_pixmap(
            0,
            0,
            rendered.as_ref(),
            &sk::PixmapPaint::default(),
            sk::Transform::identity(),
            Some(mask),
        );
    } else {
        resvg::render(svg.tree(), transform, canvas);
    }

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
    canvas: &mut sk::PixmapMut,
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

    // Without an alpha channel, every pixel is fully opaque, so overwriting
    // is always correct regardless of what's beneath.
    if raster.has_alpha() {
        return None;
    }

    let src_w = raster.width();
    let src_h = raster.height();

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

    // Source-space row/column range that maps into the clipped destination
    // rect (still a 1:1 mapping, since we required native resolution above).
    let src_y_range = (clip_y0 - dst_y0) as u32..(clip_y1 - dst_y0) as u32;
    let src_x_range = (clip_x0 - dst_x0) as u32..(clip_x1 - dst_x0) as u32;

    reserve_band_rows(raster, state, src_y_range.len() as u32, src_y_range.len() as u32);

    // Stream in just the rows this band/canvas actually needs, so a
    // full-page-sized source image never has to be fully decoded and held
    // in memory at once, and no whole-band copy of it exists either (see
    // `RasterImage::for_each_rgba_row`). Falls back to the fully decoded
    // image when the source doesn't qualify (not PNG, interlaced,
    // EXIF-rotated, etc.).
    //
    // Each row is copied in one `copy_from_slice`: an opaque source's rows
    // arrive as `[r, g, b, 255]`, which is byte-for-byte what tiny-skia
    // stores for that pixel (`PremultipliedColorU8` is `[r, g, b, a]`, and
    // premultiplying by an alpha of 1 is the identity), so no per-pixel
    // conversion is needed.
    let byte_count = src_x_range.len() * 4;
    let src_byte_offset = src_x_range.start as usize * 4;
    let dst_x = (dst_x0 + src_x_range.start as i64) as usize;
    let data = canvas.data_mut();
    let streamed =
        raster.for_each_rgba_row(src_y_range.start, src_y_range.end, |sy, row| {
            let py = (dst_y0 + sy as i64) as usize;
            let start = (py * canvas_w + dst_x) * 4;
            data[start..start + byte_count]
                .copy_from_slice(&row[src_byte_offset..src_byte_offset + byte_count]);
        });

    if streamed.is_none() {
        let dynamic = raster.dynamic();
        let pixels = canvas.pixels_mut();
        for sy in src_y_range {
            let py = (dst_y0 + sy as i64) as usize;
            for sx in src_x_range.clone() {
                let px = (dst_x0 + sx as i64) as usize;
                let Rgba([r, g, b, _]) = dynamic.get_pixel(sx, sy);
                pixels[py * canvas_w + px] =
                    sk::ColorU8::from_rgba(r, g, b, 255).premultiply();
            }
        }
    }

    Some(())
}

/// Fast path for a native-resolution raster image with an alpha channel.
///
/// Browser canvas PNGs commonly carry an alpha channel even when every pixel
/// is opaque. Such images cannot use [`try_blit_opaque`], but they still do
/// not need a full-size texture: decode the rows visible in this band and
/// blend them directly into the destination canvas.
fn try_blit_native_alpha(
    canvas: &mut sk::PixmapMut,
    state: &State,
    image: &Image,
    view_width: f32,
    view_height: f32,
) -> Option<()> {
    if state.mask.is_some() {
        return None;
    }

    let ts = state.transform;
    if ts.kx != 0.0 || ts.ky != 0.0 || ts.sx <= 0.0 || ts.sy <= 0.0 {
        return None;
    }

    let ImageKind::Raster(raster) = image.kind() else { return None };
    if !raster.has_alpha() {
        return None;
    }

    let src_w = raster.width();
    let src_h = raster.height();
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

    let dst_x0 = rx0 as i64;
    let dst_y0 = ry0 as i64;
    let clip_x0 = dst_x0.max(0);
    let clip_y0 = dst_y0.max(0);
    let clip_x1 = (dst_x0 + dst_w).min(canvas.width() as i64);
    let clip_y1 = (dst_y0 + dst_h).min(canvas.height() as i64);
    if clip_x0 >= clip_x1 || clip_y0 >= clip_y1 {
        return Some(());
    }

    let src_y_range = (clip_y0 - dst_y0) as u32..(clip_y1 - dst_y0) as u32;
    let src_x_range = (clip_x0 - dst_x0) as u32..(clip_x1 - dst_x0) as u32;

    reserve_band_rows(raster, state, src_y_range.len() as u32, src_y_range.len() as u32);

    let canvas_w = canvas.width() as usize;
    let pixels = bytemuck::cast_slice_mut::<u8, u32>(canvas.data_mut());
    let mut blend_row = |sy: u32, row: &[u8]| {
        let py = (dst_y0 + sy as i64) as usize;
        for sx in src_x_range.clone() {
            let idx = sx as usize * 4;
            let a = row[idx + 3] as u32;
            let r = (row[idx] as u32 * a + 127) / 255;
            let g = (row[idx + 1] as u32 * a + 127) / 255;
            let b = (row[idx + 2] as u32 * a + 127) / 255;
            let src = r | (g << 8) | (b << 16) | (a << 24);
            let dst = &mut pixels[py * canvas_w + (dst_x0 + sx as i64) as usize];
            *dst = src + alpha_mul(*dst, 256 - (src >> 24));
        }
    };

    if raster
        .for_each_rgba_row(src_y_range.start, src_y_range.end, |sy, row| {
            blend_row(sy, row);
        })
        .is_none()
    {
        let dynamic = raster.dynamic();
        for sy in src_y_range.clone() {
            let py = (dst_y0 + sy as i64) as usize;
            for sx in src_x_range.clone() {
                let Rgba([r, g, b, a]) = dynamic.get_pixel(sx, sy);
                let a = a as u32;
                let src = ((r as u32 * a + 127) / 255)
                    | (((g as u32 * a + 127) / 255) << 8)
                    | (((b as u32 * a + 127) / 255) << 16)
                    | (a << 24);
                let dst = &mut pixels[py * canvas_w + (dst_x0 + sx as i64) as usize];
                *dst = src + alpha_mul(*dst, 256 - (src >> 24));
            }
        }
    }

    Some(())
}

fn alpha_mul(color: u32, scale: u32) -> u32 {
    let mask = 0xff00ff;
    let rb = ((color & mask) * scale) >> 8;
    let ag = ((color >> 8) & mask) * scale;
    (rb & mask) | (ag & !mask)
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
    canvas: &mut sk::PixmapMut,
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
    let (src_w, src_h) = (raster.width(), raster.height());

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

    // Try to decode only the source rows this crop actually needs, so a
    // full-page-sized source image never has to be fully decoded and held
    // in memory at once (see `RasterImage::decode_row_range`). Falls
    // back to the fully decoded, fully converted buffer when the source
    // doesn't qualify (not PNG, interlaced, EXIF-rotated, etc.).
    //
    // A source without an alpha channel is decoded and resized as three
    // channels rather than four, which is a quarter less memory in both the
    // decoded region and the resize target -- the two largest buffers this
    // path holds, and both proportional to the band size.
    let channels: u8 =
        if !raster.has_alpha() && raster.supports_row_range(3) { 3 } else { 4 };
    let pixel_type = if channels == 3 { PixelType::U8x3 } else { PixelType::U8x4 };

    let row_lo = crop_top.floor().max(0.0) as u32;
    let row_hi = (crop_top + crop_height).ceil().min(src_h as f64) as u32;

    reserve_band_rows(
        raster,
        state,
        (local_y1 - local_y0) as u32,
        row_hi.saturating_sub(row_lo),
    );

    let mut resized = FirImage::new(crop_w, crop_h, pixel_type);
    // `fast_image_resize` premultiplies by alpha before convolving and
    // divides it back out afterwards, so transparent pixels don't bleed their
    // color into their neighbors. For a source whose alpha is uniformly 255
    // both passes are exact identities -- multiplying by one, then dividing
    // by one -- so they can be skipped outright, saving two full passes over
    // the region for the common case of an opaque photographic background.
    let opts = ResizeOptions::new().resize_alg(alg).use_alpha(raster.has_alpha());
    if let Some(region) = raster.decode_row_range(row_lo, row_hi, channels) {
        let region_h = row_hi - row_lo;
        let region_img =
            FirImage::from_vec_u8(src_w, region_h, region, pixel_type).ok()?;
        // `row_hi`/the region's actual height are clamped to `src_h`, but
        // `crop_height` (and, symmetrically, `crop_width` against `src_w`)
        // are derived from the destination-side margin before that clamp,
        // so at the image's bottom/right edge the nominal crop rect can
        // extend past the decoded region -- clamp it back to what was
        // actually decoded, matching the real (already-clamped) source
        // bounds `fast_image_resize` would otherwise reject.
        let local_crop_top = crop_top - row_lo as f64;
        let crop_height = crop_height.min(region_h as f64 - local_crop_top);
        let crop_width = crop_width.min(src_w as f64 - crop_left);
        let opts = opts.crop(crop_left, local_crop_top, crop_width, crop_height);
        Resizer::new().resize(&region_img, &mut resized, &opts).ok()?;
    } else {
        // `supports_row_range` already reported that the row-range path
        // applies whenever `channels` is 3, so in practice this fallback
        // only runs with a 4-channel `resized`. If a decode nonetheless
        // fails part-way through the file, `resize` rejects the pixel-type
        // mismatch and the `?` below hands the image to the general path,
        // which is the same fallback as any other unsupported source.
        let src = raster.rgba8();
        let opts = opts.crop(crop_left, crop_top, crop_width, crop_height);
        Resizer::new().resize(src.as_ref(), &mut resized, &opts).ok()?;
    }

    let (tile_w, tile_h) = ((clip_x1 - clip_x0) as u32, (clip_y1 - clip_y0) as u32);
    let offset_x = (local_x0 as u32) - start_x;
    let offset_y = (local_y0 as u32) - start_y;
    let buf = resized.buffer();

    // With no mask and no alpha, `src over dst` is just `src`, so the
    // resized pixels can go straight into the canvas -- skipping both a
    // second band-sized pixmap and the compositing pass over it. This is the
    // common case for a full-bleed opaque background.
    if state.mask.is_none() && channels == 3 {
        let canvas_w = canvas.width() as usize;
        let data = canvas.data_mut();
        for row in 0..tile_h {
            let src_start = (((offset_y + row) * crop_w + offset_x) * 3) as usize;
            let src_row = &buf[src_start..src_start + (tile_w as usize) * 3];
            let dest_start =
                ((clip_y0 as usize + row as usize) * canvas_w + clip_x0 as usize) * 4;
            let dest_row = &mut data[dest_start..dest_start + (tile_w as usize) * 4];
            for (chunk, dest) in src_row.chunks_exact(3).zip(dest_row.chunks_exact_mut(4))
            {
                dest[..3].copy_from_slice(chunk);
                dest[3] = 255;
            }
        }
        return Some(());
    }

    eprintln!(
        "DBG dst=({dst_w},{dst_h}) dst0=({dst_x0},{dst_y0}) clip=({clip_x0},{clip_y0},{clip_x1},{clip_y1}) local=({local_x0},{local_y0},{local_x1},{local_y1}) start=({start_x},{start_y}) end=({end_x},{end_y}) crop=({crop_w},{crop_h}) tile=({tile_w},{tile_h}) off=({offset_x},{offset_y}) buf={} canvas=({},{})",
        buf.len(),
        canvas.width(),
        canvas.height()
    );
    let mut tile = sk::Pixmap::new(tile_w, tile_h)?;
    for row in 0..tile_h {
        let row_start = ((offset_y + row) as usize * crop_w as usize + offset_x as usize)
            * channels as usize;
        let row_bytes = &buf[row_start..row_start + tile_w as usize * channels as usize];
        let dest_start = (row * tile_w) as usize;
        let dest_row = &mut tile.pixels_mut()[dest_start..dest_start + tile_w as usize];
        for (chunk, dest) in row_bytes.chunks_exact(channels as usize).zip(dest_row) {
            let alpha = if channels == 4 { chunk[3] } else { 255 };
            *dest =
                sk::ColorU8::from_rgba(chunk[0], chunk[1], chunk[2], alpha).premultiply();
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

/// Rotation/skew counterpart to [`try_blit_resized_axis_aligned`]: bounds
/// memory the same way (a small cropped tile instead of a whole-image
/// texture) for a row-range-decodable raster image, even when the
/// placement's transform isn't axis-aligned.
///
/// Without this, any rotated or skewed raster image (however slightly --
/// even a fraction of a degree) fell through to the general path below,
/// which always builds a texture sized to the *whole placed image* via
/// `build_texture`/`RasterImage::rgba8`, regardless of how much of it actually
/// overlaps the current canvas. For a page rendered in bands (see
/// `render_band`), that's a whole-page-sized allocation on the very first
/// band -- comemo then reuses it for subsequent bands (so it only happens
/// once), but that one allocation alone can already exceed a tight
/// `--max-memory` budget for a large full-bleed background, defeating
/// banding entirely for exactly the scenario it's meant to help.
///
/// Returns `None` (with no side effects) whenever a precondition doesn't
/// hold (not a raster image, or the source doesn't qualify for
/// [`typst_library::visualize::RasterImage::decode_rgba_row_range`] --
/// interlaced, EXIF-rotated, or
/// non-PNG), so callers fall back to the general path unchanged.
fn try_blit_resized_general(
    canvas: &mut sk::PixmapMut,
    state: &State,
    image: &Image,
    view_width: f32,
    view_height: f32,
) -> Option<()> {
    let ts = state.transform;
    let ImageKind::Raster(raster) = image.kind() else { return None };
    let (src_w, src_h) = (raster.width(), raster.height());

    fn minmax(points: &[sk::Point]) -> (f32, f32, f32, f32) {
        let (mut min_x, mut max_x) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut min_y, mut max_y) = (f32::INFINITY, f32::NEG_INFINITY);
        for p in points {
            min_x = min_x.min(p.x);
            max_x = max_x.max(p.x);
            min_y = min_y.min(p.y);
            max_y = max_y.max(p.y);
        }
        (min_x, max_x, min_y, max_y)
    }

    // Canvas-space bounding box of the transformed placement rect, clipped
    // to the canvas -- for a page rendered in bands, the canvas only covers
    // one band, so a full-page image only partially (or not at all)
    // overlaps it.
    let mut corners = [
        sk::Point { x: 0.0, y: 0.0 },
        sk::Point { x: view_width, y: 0.0 },
        sk::Point { x: 0.0, y: view_height },
        sk::Point { x: view_width, y: view_height },
    ];
    ts.map_points(&mut corners);
    let (min_x, max_x, min_y, max_y) = minmax(&corners);

    let clip_x0 = (min_x.floor() as i64).max(0);
    let clip_y0 = (min_y.floor() as i64).max(0);
    let clip_x1 = (max_x.ceil() as i64).min(canvas.width() as i64);
    let clip_y1 = (max_y.ceil() as i64).min(canvas.height() as i64);
    if clip_x0 >= clip_x1 || clip_y0 >= clip_y1 {
        // No overlap with this band/canvas at all.
        return Some(());
    }

    // Map the clipped canvas rect back into local placement space (via the
    // inverse transform) to find which part of the image this band
    // actually needs.
    let inv = ts.invert()?;
    let mut canvas_corners = [
        sk::Point { x: clip_x0 as f32, y: clip_y0 as f32 },
        sk::Point { x: clip_x1 as f32, y: clip_y0 as f32 },
        sk::Point { x: clip_x0 as f32, y: clip_y1 as f32 },
        sk::Point { x: clip_x1 as f32, y: clip_y1 as f32 },
    ];
    inv.map_points(&mut canvas_corners);
    let (local_min_x, local_max_x, local_min_y, local_max_y) = minmax(&canvas_corners);

    let local_x0 = local_min_x.clamp(0.0, view_width);
    let local_x1 = local_max_x.clamp(0.0, view_width);
    let local_y0 = local_min_y.clamp(0.0, view_height);
    let local_y1 = local_max_y.clamp(0.0, view_height);
    if local_x1 <= local_x0 || local_y1 <= local_y0 {
        return Some(());
    }

    // The same target-resolution computation the general (unbounded) path
    // uses below, needed here to know the local-space-to-source-pixel
    // ratio. See the comment there for the math's origin.
    let theta = libm::atan2f(-ts.kx, ts.sx);
    let prefer_sin = libm::sinf(theta).abs() > std::f32::consts::FRAC_1_SQRT_2;
    let scale = f32::abs(if prefer_sin {
        ts.kx / libm::sinf(theta)
    } else {
        ts.sx / libm::cosf(theta)
    });
    let aspect = src_w as f32 / src_h as f32;
    let full_w = (scale * view_width.max(aspect * view_height)).ceil().max(1.0) as u32;
    let full_h = ((full_w as f32) / aspect).ceil().max(1.0) as u32;

    let upscale = full_w > src_w || full_h > src_h;
    let (alg, support) = match image.scaling() {
        Smart::Custom(ImageScaling::Pixelated) => (ResizeAlg::Nearest, 0.0),
        _ if upscale => (ResizeAlg::Convolution(FilterType::CatmullRom), 2.0),
        _ => (ResizeAlg::Convolution(FilterType::Lanczos3), 3.0),
    };

    // Map the needed local rect into texture-pixel space (the space
    // `build_texture(image, full_w, full_h)` would produce), expanded by
    // the resample filter's kernel support on each side, same idea as
    // `try_blit_resized_axis_aligned`.
    let scale_x = src_w as f64 / full_w as f64;
    let scale_y = src_h as f64 / full_h as f64;
    // A larger constant than `try_blit_resized_axis_aligned` uses: unlike
    // that function's pixel-grid-aligned tiles, a rotated/skewed tile's
    // needed region comes from inverse-transforming the canvas clip rect,
    // which can be off by a subpixel amount at the tile's edge -- extra
    // slack here avoids a visible seam where the image's own (possibly
    // rotated) edge crosses a band boundary.
    let margin_x = (support * scale_x.max(1.0)).ceil() as i64 + 6;
    let margin_y = (support * scale_y.max(1.0)).ceil() as i64 + 6;

    let tex_x0 = local_x0 as f64 * full_w as f64 / view_width as f64;
    let tex_x1 = local_x1 as f64 * full_w as f64 / view_width as f64;
    let tex_y0 = local_y0 as f64 * full_h as f64 / view_height as f64;
    let tex_y1 = local_y1 as f64 * full_h as f64 / view_height as f64;

    let start_x = ((tex_x0.floor() as i64) - margin_x).clamp(0, full_w as i64) as u32;
    let start_y = ((tex_y0.floor() as i64) - margin_y).clamp(0, full_h as i64) as u32;
    let end_x = ((tex_x1.ceil() as i64) + margin_x).clamp(0, full_w as i64) as u32;
    let end_y = ((tex_y1.ceil() as i64) + margin_y).clamp(0, full_h as i64) as u32;
    let (crop_w, crop_h) = (end_x - start_x, end_y - start_y);
    if crop_w == 0 || crop_h == 0 {
        return Some(());
    }

    let crop_left = start_x as f64 * scale_x;
    let crop_top = start_y as f64 * scale_y;
    let crop_width = crop_w as f64 * scale_x;
    let crop_height = crop_h as f64 * scale_y;

    let row_lo = crop_top.floor().max(0.0) as u32;
    let row_hi = (crop_top + crop_height).ceil().min(src_h as f64) as u32;

    reserve_band_rows(
        raster,
        state,
        (local_y1 - local_y0) as u32,
        row_hi.saturating_sub(row_lo),
    );

    let region = raster.decode_rgba_row_range(row_lo, row_hi)?;
    let region_h = row_hi - row_lo;
    let region_img =
        FirImage::from_vec_u8(src_w, region_h, region, PixelType::U8x4).ok()?;

    // See the matching comment in `try_blit_resized_axis_aligned`: the
    // nominal crop rect can extend past what was actually decoded at the
    // image's bottom/right edge, so clamp it back.
    let local_crop_top = crop_top - row_lo as f64;
    let crop_height = crop_height.min(region_h as f64 - local_crop_top);
    let crop_width = crop_width.min(src_w as f64 - crop_left);

    let mut resized = FirImage::new(crop_w, crop_h, PixelType::U8x4);
    // See the matching comment in `try_blit_resized_axis_aligned`.
    let opts = ResizeOptions::new()
        .resize_alg(alg)
        .use_alpha(raster.has_alpha())
        .crop(crop_left, local_crop_top, crop_width, crop_height);
    Resizer::new().resize(&region_img, &mut resized, &opts).ok()?;

    drop(region_img);

    // Reuse the resize allocation as the texture instead of keeping a second
    // RGBA buffer alive during compositing.
    let mut tile =
        sk::Pixmap::from_vec(resized.into_vec(), IntSize::from_wh(crop_w, crop_h)?)?;
    for pixel in tile.data_mut().chunks_exact_mut(4) {
        let color =
            sk::ColorU8::from_rgba(pixel[0], pixel[1], pixel[2], pixel[3]).premultiply();
        pixel.copy_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }

    // Paint the small tile with the *same* affine transform the unbounded
    // path uses, just restricted to the local-space sub-rect this tile
    // actually covers -- `fill_rect` takes care of applying `ts` (rotation
    // included) to both `rect` and the pattern's sample space, and of
    // clipping to the canvas, exactly as it already did for the full-image
    // case.
    let sub_x0 = start_x as f32 * view_width / full_w as f32;
    let sub_y0 = start_y as f32 * view_height / full_h as f32;
    let sub_w = crop_w as f32 * view_width / full_w as f32;
    let sub_h = crop_h as f32 * view_height / full_h as f32;
    let paint_scale_x = view_width / full_w as f32;
    let paint_scale_y = view_height / full_h as f32;

    let paint = sk::Paint {
        shader: sk::Pattern::new(
            tile.as_ref(),
            sk::SpreadMode::Pad,
            sk::FilterQuality::Nearest,
            1.0,
            sk::Transform::from_scale(paint_scale_x, paint_scale_y)
                .post_concat(sk::Transform::from_translate(sub_x0, sub_y0)),
        ),
        ..Default::default()
    };
    let rect = sk::Rect::from_xywh(sub_x0, sub_y0, sub_w, sub_h)?;
    canvas.fill_rect(rect, &paint, ts, state.mask);

    Some(())
}

/// Reserves a retained-row window on `raster` large enough to cover the whole
/// band this canvas belongs to, rather than just this canvas's own rows.
///
/// The tiles of one band are rendered concurrently
/// (see `typst_render::render_band_into`), so they reach a raster image's
/// single sequential row cursor in an arbitrary order: whichever tile arrives
/// first pulls the cursor down to its own rows, and every tile above it then
/// asks for rows the cursor has already passed. Without a window that spans
/// the band, each of those would restart decompression from row 0 and make a
/// tiled band cost `O(tiles * height)` row decodes instead of `O(height)`.
///
/// `dst_rows` and `src_rows` are this request's own destination and source row
/// counts, so the ratio between them extrapolates the band's height into
/// source rows without this function needing to know which coordinate space
/// the caller works in.
fn reserve_band_rows(raster: &RasterImage, state: &State, dst_rows: u32, src_rows: u32) {
    if state.band_rows == 0 || dst_rows == 0 {
        return;
    }
    // `src_rows` covers this request's own rows plus the resample filter's
    // margin on both sides, so the ratio is already an over-estimate of the
    // band's source rows and needs no further slack added on top.
    let per_row = src_rows as f64 / dst_rows as f64;
    let rows = (state.band_rows as f64 * per_row).ceil();
    raster.reserve_retained_rows(rows.min(u32::MAX as f64) as u32);
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
                let src = raster.rgba8();
                let mut dst = FirImage::new(w, h, PixelType::U8x4);
                // See the matching comment in
                // `try_blit_resized_axis_aligned` for `use_alpha`.
                let opts =
                    ResizeOptions::new().resize_alg(alg).use_alpha(raster.has_alpha());
                Resizer::new().resize(src.as_ref(), &mut dst, &opts).ok()?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder;
    use typst_library::foundations::Bytes;
    use typst_library::visualize::ExchangeFormat;

    fn solid_png(alpha: bool) -> Image {
        let mut data = Vec::new();
        let pixel = if alpha { &[120, 80, 40, 128][..] } else { &[120, 80, 40][..] };
        image::codecs::png::PngEncoder::new(&mut data)
            .write_image(
                &pixel.repeat(32 * 32),
                32,
                32,
                if alpha {
                    image::ExtendedColorType::Rgba8
                } else {
                    image::ExtendedColorType::Rgb8
                },
            )
            .unwrap();
        Image::plain(RasterImage::plain(Bytes::new(data), ExchangeFormat::Png).unwrap())
    }

    #[test]
    fn resized_masked_png_matches_unmasked_composite() {
        for alpha in [false, true] {
            for dimension in [16, 64] {
                let image = solid_png(alpha);
                let mut mask = sk::Mask::new(dimension, dimension).unwrap();
                mask.data_mut().fill(255);
                let state = State { mask: Some(&mask), ..State::default() };
                let mut masked = sk::Pixmap::new(dimension, dimension).unwrap();
                let mut reference = masked.clone();
                masked.fill(sk::Color::WHITE);
                reference.fill(sk::Color::WHITE);
                try_blit_resized_axis_aligned(
                    &mut masked.as_mut(),
                    &state,
                    &image,
                    dimension as f32,
                    dimension as f32,
                )
                .unwrap();
                try_blit_resized_axis_aligned(
                    &mut reference.as_mut(),
                    &State::default(),
                    &image,
                    dimension as f32,
                    dimension as f32,
                )
                .unwrap();
                assert_eq!(masked.data(), reference.data());
                let pixel = masked.pixel(0, 0).unwrap();
                if !alpha {
                    assert_eq!(
                        (pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()),
                        (120, 80, 40, 255)
                    );
                }
            }
        }
    }
}
