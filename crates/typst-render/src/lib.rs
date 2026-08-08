//! Rendering of Typst documents into raster images.

mod image;
mod paint;
mod shape;
mod text;

use tiny_skia as sk;
use typst_layout::{Page, PagedDocument};
use typst_library::layout::{
    Abs, Axes, Frame, FrameItem, FrameKind, GroupItem, Point, Sides, Size, Transform,
};
use typst_library::visualize::{Color, Geometry, Paint};
use typst_utils::Scalar;

/// The page geometry shared by [`render`] and [`render_band`].
struct PageGeometry {
    bleed: Sides<Abs>,
    size: Size,
    pixel_per_pt: f32,
    pxw: u32,
    pxh: u32,
}

fn page_geometry(page: &Page, opts: &RenderOptions) -> PageGeometry {
    let bleed = if opts.render_bleed { page.bleed } else { Sides::default() };
    let size = page.frame.size() + bleed.sum_by_axis();
    let pixel_per_pt = opts.pixel_per_pt.get() as f32;
    let pxw = (pixel_per_pt * size.x.to_f32()).round().max(1.0) as u32;
    let pxh = (pixel_per_pt * size.y.to_f32()).round().max(1.0) as u32;
    PageGeometry { bleed, size, pixel_per_pt, pxw, pxh }
}

/// Paints the page's background fill (solid color or gradient/pattern) onto
/// `canvas`, which may be the full page or a single band of it.
fn paint_background(canvas: &mut sk::Pixmap, state: State, page: &Page, size: Size) {
    if let Some(fill) = page.fill_or_white() {
        if let Paint::Solid(color) = fill {
            canvas.fill(paint::to_sk_color(color.to_process()));
        } else {
            let rect = Geometry::Rect(size).filled(fill);
            shape::render_shape(canvas, state, &rect);
        }
    }
}

/// Returns whether any paint in `page` (background, shape fill/stroke, or
/// text fill/stroke) is a gradient or tiling pattern.
///
/// [`render_band`] renders each band with its own transform, offset by the
/// band's absolute position. A `relative: "parent"` gradient or pattern
/// computes its placement as `container_transform.post_concat(transform
/// .invert())`, which should cancel that offset out algebraically -- but
/// since `sk::Transform` is f32-only, subtracting two large,
/// nearly-equal values loses precision in the low bits where the true
/// (small) result lives, producing a small but real color error at typical
/// page sizes. Callers should render pages for which this returns `true` as
/// a single band (i.e. the whole page), matching the un-banded behavior.
pub fn uses_relative_paint(page: &Page) -> bool {
    fn is_relative_paint(paint: &Paint) -> bool {
        matches!(paint, Paint::Gradient(_) | Paint::Tiling(_))
    }

    fn check_frame(frame: &Frame) -> bool {
        frame.items().any(|(_, item)| match item {
            FrameItem::Group(group) => check_frame(&group.frame),
            FrameItem::Text(text) => {
                is_relative_paint(&text.fill)
                    || text
                        .stroke
                        .as_ref()
                        .is_some_and(|stroke| is_relative_paint(&stroke.paint))
            }
            FrameItem::Shape(shape, _) => {
                shape.fill.as_ref().is_some_and(is_relative_paint)
                    || shape
                        .stroke
                        .as_ref()
                        .is_some_and(|stroke| is_relative_paint(&stroke.paint))
            }
            FrameItem::Image(..) | FrameItem::Link(..) | FrameItem::Tag(..) => false,
        })
    }

    page.fill_or_white().is_some_and(|fill| is_relative_paint(&fill))
        || check_frame(&page.frame)
}

/// Returns the device-pixel dimensions that [`render`] (or [`render_band`])
/// would produce for `page` at the given options, without rendering
/// anything. Useful for a caller that wants to pick a band size for
/// [`render_band`] ahead of time.
pub fn pixel_dimensions(page: &Page, opts: &RenderOptions) -> (u32, u32) {
    let geo = page_geometry(page, opts);
    (geo.pxw, geo.pxh)
}

/// Export a page into a raster image.
///
/// This renders the page at the given number of pixels per point and returns
/// the resulting `tiny-skia` pixel buffer.
#[typst_macros::time(name = "render")]
pub fn render(page: &Page, opts: &RenderOptions) -> sk::Pixmap {
    let geo = page_geometry(page, opts);
    let ts = sk::Transform::from_scale(geo.pixel_per_pt, geo.pixel_per_pt);
    let state = State::new(geo.size, ts, geo.pixel_per_pt);

    let mut canvas = sk::Pixmap::new(geo.pxw, geo.pxh).unwrap();
    paint_background(&mut canvas, state, page, geo.size);

    let state = state.pre_translate(Point { x: geo.bleed.left, y: geo.bleed.top });
    render_frame(&mut canvas, state, &page.frame);

    canvas
}

/// Render a horizontal band of a page into a small pixmap, covering device
/// pixel rows `y_offset_px..y_offset_px + band_height_px` of the full page.
///
/// This lets a caller export a very large page (e.g. a large-format poster)
/// without holding the full-page canvas in memory at once: render each band,
/// stream it out (e.g. to a PNG encoder), and drop it before rendering the
/// next one. Each call re-walks the whole frame, so this trades some
/// redundant tree-walking for materializing only `band_height_px` rows of
/// pixels at a time instead of the whole page.
#[typst_macros::time(name = "render band")]
pub fn render_band(
    page: &Page,
    opts: &RenderOptions,
    y_offset_px: u32,
    band_height_px: u32,
) -> sk::Pixmap {
    let geo = page_geometry(page, opts);
    let ts = sk::Transform::from_scale(geo.pixel_per_pt, geo.pixel_per_pt)
        .post_translate(0.0, -(y_offset_px as f32));
    let state = State::new(geo.size, ts, geo.pixel_per_pt);

    let mut canvas = sk::Pixmap::new(geo.pxw, band_height_px).unwrap();
    paint_background(&mut canvas, state, page, geo.size);

    let state = state.pre_translate(Point { x: geo.bleed.left, y: geo.bleed.top });
    render_frame(&mut canvas, state, &page.frame);

    canvas
}

/// Export a document with potentially multiple pages into a single raster image.
pub fn render_merged(
    document: &PagedDocument,
    opts: &RenderOptions,
    gap: Abs,
    fill: Option<Color>,
) -> sk::Pixmap {
    let pixel_per_pt = opts.pixel_per_pt.get() as f32;
    let gap = (pixel_per_pt * gap.to_f32()).round() as u32;

    // Compute the merged canvas's size from each page's pixel dimensions
    // alone, without rendering anything -- so we don't need to hold every
    // page's full canvas in memory just to find their sizes. Pages are then
    // rendered and drawn one at a time below, so at most one page's canvas
    // (rather than every page's) is resident alongside the merged canvas.
    let sizes: Vec<(u32, u32)> =
        document.pages().iter().map(|page| pixel_dimensions(page, opts)).collect();
    let pxw = sizes.iter().map(|&(w, _)| w).max().unwrap_or_default();
    let pxh = sizes.iter().map(|&(_, h)| h).sum::<u32>()
        + gap * sizes.len().saturating_sub(1) as u32;

    let mut canvas = sk::Pixmap::new(pxw, pxh).unwrap();
    if let Some(fill) = fill {
        canvas.fill(paint::to_sk_color(fill.to_process()));
    }

    let mut y = 0;
    for page in document.pages() {
        let pixmap = render(page, opts);
        canvas.draw_pixmap(
            0,
            y as i32,
            pixmap.as_ref(),
            &sk::PixmapPaint::default(),
            sk::Transform::identity(),
            None,
        );

        y += pixmap.height() + gap;
    }

    canvas
}

/// Settings for raster image export.
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct RenderOptions {
    /// Controls the scale of the rendered output in pixels per typographic
    /// point. By default, a value of `1.0` is used, meaning one pixel is
    /// generated per point. Increasing this value produces higher-resolution
    /// images, while lower values reduce the output size and rendering cost.
    /// This can be useful when adjusting the final image quality for display or
    /// printing purposes.
    pub pixel_per_pt: Scalar,
    /// By default, rendered pages are bounded to the page size. In some
    /// circumstances, such as when preparing documents for print, it may be
    /// desirable to include content beyond these bounds to account for bleed
    /// margins. This field allows expanding the rendered area to include such
    /// bleed.
    pub render_bleed: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            pixel_per_pt: Scalar::new(2.0),
            render_bleed: false,
        }
    }
}

/// Additional metadata carried through the rendering process.
#[derive(Default, Copy, Clone)]
struct State<'a> {
    /// The transform of the current item.
    transform: sk::Transform,
    /// The transform of the first hard frame in the hierarchy.
    container_transform: sk::Transform,
    /// The mask of the current item.
    mask: Option<&'a sk::Mask>,
    /// The pixel per point ratio.
    pixel_per_pt: f32,
    /// The size of the first hard frame in the hierarchy.
    size: Size,
}

impl<'a> State<'a> {
    fn new(size: Size, transform: sk::Transform, pixel_per_pt: f32) -> Self {
        Self {
            size,
            transform,
            container_transform: transform,
            pixel_per_pt,
            ..Default::default()
        }
    }

    /// Pre translate the current item's transform.
    fn pre_translate(self, pos: Point) -> Self {
        Self {
            transform: self.transform.pre_translate(pos.x.to_f32(), pos.y.to_f32()),
            ..self
        }
    }

    fn pre_scale(self, scale: Axes<Abs>) -> Self {
        Self {
            transform: self.transform.pre_scale(scale.x.to_f32(), scale.y.to_f32()),
            ..self
        }
    }

    /// Pre concat the current item's transform.
    fn pre_concat(self, transform: sk::Transform) -> Self {
        Self {
            transform: self.transform.pre_concat(transform),
            ..self
        }
    }

    /// Sets the current mask.
    ///
    /// If no mask is provided, the parent mask is used.
    fn with_mask(self, mask: Option<&'a sk::Mask>) -> State<'a> {
        State { mask: mask.or(self.mask), ..self }
    }

    /// Sets the size of the first hard frame in the hierarchy.
    fn with_size(self, size: Size) -> Self {
        Self { size, ..self }
    }

    /// Pre concat the container's transform.
    fn pre_concat_container(self, transform: sk::Transform) -> Self {
        Self {
            container_transform: self.container_transform.pre_concat(transform),
            ..self
        }
    }
}

/// Render a frame into the canvas.
fn render_frame(canvas: &mut sk::Pixmap, state: State, frame: &Frame) {
    for (pos, item) in frame.items() {
        match item {
            FrameItem::Group(group) => {
                render_group(canvas, state, *pos, group);
            }
            FrameItem::Text(text) => {
                text::render_text(canvas, state.pre_translate(*pos), text);
            }
            FrameItem::Shape(shape, _) => {
                shape::render_shape(canvas, state.pre_translate(*pos), shape);
            }
            FrameItem::Image(image, size, _) => {
                image::render_image(canvas, state.pre_translate(*pos), image, *size);
            }
            FrameItem::Link(_, _) => {}
            FrameItem::Tag(_) => {}
        }
    }
}

/// Render a group frame with optional transform and clipping into the canvas.
fn render_group(canvas: &mut sk::Pixmap, state: State, pos: Point, group: &GroupItem) {
    let sk_transform = to_sk_transform(&group.transform);
    let state = match group.frame.kind() {
        FrameKind::Soft => state.pre_translate(pos).pre_concat(sk_transform),
        FrameKind::Hard => state
            .pre_translate(pos)
            .pre_concat(sk_transform)
            .pre_concat_container(
                state
                    .transform
                    .post_concat(state.container_transform.invert().unwrap()),
            )
            .pre_concat_container(to_sk_transform(&Transform::translate(pos.x, pos.y)))
            .pre_concat_container(sk_transform)
            .with_size(group.frame.size()),
    };

    let mut mask = state.mask;
    let storage;
    if let Some(clip_curve) = group.clip.as_ref()
        && let Some(path) = shape::convert_curve(clip_curve)
            .and_then(|path| path.transform(state.transform))
    {
        if let Some(mask) = mask {
            let mut mask = mask.clone();
            mask.intersect_path(
                &path,
                sk::FillRule::default(),
                true,
                sk::Transform::default(),
            );
            storage = mask;
        } else {
            let pxw = canvas.width();
            let pxh = canvas.height();
            let Some(mut mask) = sk::Mask::new(pxw, pxh) else {
                // Fails if clipping rect is empty. In that case we just
                // clip everything by returning.
                return;
            };

            mask.fill_path(
                &path,
                sk::FillRule::default(),
                true,
                sk::Transform::default(),
            );
            storage = mask;
        };

        mask = Some(&storage);
    }

    render_frame(canvas, state.with_mask(mask), &group.frame);
}

fn to_sk_transform(transform: &Transform) -> sk::Transform {
    let Transform { sx, ky, kx, sy, tx, ty } = *transform;
    sk::Transform::from_row(
        sx.get() as _,
        ky.get() as _,
        kx.get() as _,
        sy.get() as _,
        tx.to_f32(),
        ty.to_f32(),
    )
}

/// Additional methods for [`Abs`].
trait AbsExt {
    /// Convert to a number of points as f32.
    fn to_f32(self) -> f32;
}

impl AbsExt for Abs {
    fn to_f32(self) -> f32 {
        self.to_pt() as f32
    }
}
