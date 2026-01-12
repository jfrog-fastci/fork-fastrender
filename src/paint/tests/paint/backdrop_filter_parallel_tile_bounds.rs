use crate::geometry::Rect;
use crate::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, ResolvedFilter,
  StackingContextItem,
};
use crate::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use crate::style::types::{BackfaceVisibility, TransformStyle};
use crate::text::font_loader::FontContext;
use crate::Rgba;

#[test]
fn backdrop_filter_parallel_tiles_do_not_index_past_backdrop_root_bounds() {
  // Regression: when painting in tile-parallel mode, a stacking context that spans multiple tiles
  // can have a local (tile-space) origin that is negative for some tiles. Backdrop-filter region
  // bounds are still computed in the shared coordinate space, so the filtered region can extend
  // outside the tile's Backdrop Root pixmap. Ensure we clamp reads/writes so mask application does
  // not panic (OOB indexing).
  let mut list = DisplayList::new();

  // High-contrast backdrop so blur changes pixels near the seam.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(0.0, 0.0, 256.0, 512.0),
    color: Rgba::from_rgba8(255, 0, 0, 255),
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(256.0, 0.0, 256.0, 512.0),
    color: Rgba::from_rgba8(0, 0, 255, 255),
  }));

  let bounds = Rect::from_xywh(0.0, 0.0, 512.0, 512.0);
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    // The stacking context itself uses backdrop-filter, but setting this to true ensures the
    // renderer allocates the necessary backdrop surfaces in parallel mode.
    has_backdrop_sensitive_descendants: true,
    bounds,
    plane_rect: bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: true,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: vec![ResolvedFilter::Blur(20.0)],
    radii: BorderRadii::uniform(40.0),
    mask: None,
    mask_border: None,
    has_clip_path: false,
  }));

  // Semi-transparent fill so the filtered backdrop is visible.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::from_rgba8(255, 255, 255, 64),
  }));
  list.push(DisplayItem::PopStackingContext);

  let report = DisplayListRenderer::new(512, 512, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .with_parallelism(PaintParallelism::enabled())
    .render_with_report(&list)
    .expect("render should not panic");
  assert!(report.parallel_used, "expected tile-parallel paint path");

  // Pixels far from the red/blue seam should stay mostly blue (plus the translucent overlay),
  // while seam pixels should pick up some red from the blur kernel.
  let pixmap = report.pixmap;
  let seam = pixmap.pixel(256, 256).expect("seam pixel");
  let far = pixmap.pixel(400, 256).expect("far pixel");
  assert!(
    seam.red() > far.red() + 5,
    "expected blur() to mix seam colors (red should increase near seam); got seam rgb({}, {}, {}), far rgb({}, {}, {})",
    seam.red(),
    seam.green(),
    seam.blue(),
    far.red(),
    far.green(),
    far.blue()
  );
}
