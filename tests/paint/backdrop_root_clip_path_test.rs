use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, ResolvedFilter, StackingContextItem,
};
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::{Rect, Rgba};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn render(has_clip_path: bool, width: u32, height: u32) -> tiny_skia::Pixmap {
  crate::rayon_test_util::init_rayon_for_tests(2);

  let mut list = DisplayList::new();
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(0.0, 0.0, width as f32, height as f32),
    color: Rgba::rgb(255, 0, 0),
  }));

  // When `has_clip_path=true`, this stacking context represents an element with a non-`none`
  // clip-path that resolves to no paint clip (e.g. `circle(0)`). It still establishes a Backdrop
  // Root, so the child backdrop filter must not sample the red background painted above.
  //
  // When `has_clip_path=false`, this is the control case (`clip-path: none`).
  let root_bounds = Rect::from_xywh(0.0, 0.0, width as f32, height as f32);
  let clip_root = StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: has_clip_path,
    has_backdrop_sensitive_descendants: true,
    bounds: root_bounds,
    plane_rect: root_bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path,
  };
  list.push(DisplayItem::PushStackingContext(clip_root));

  let overlay_bounds = Rect::from_xywh(0.0, 0.0, 40.0, 40.0);
  let overlay = StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: true,
    bounds: overlay_bounds,
    plane_rect: overlay_bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: vec![ResolvedFilter::Invert(1.0)],
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  };
  list.push(DisplayItem::PushStackingContext(overlay));
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopStackingContext);

  DisplayListRenderer::new(width, height, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .with_parallelism(PaintParallelism::disabled())
    .render(&list)
    .expect("render")
}

#[test]
fn backdrop_filter_does_not_sample_above_clip_path_backdrop_root() {
  let pixmap = render(true, 60, 60);

  // Pixel inside the backdrop-filter element should remain the page background (red) because the
  // clip-path ancestor establishes a Backdrop Root.
  assert_eq!(pixel(&pixmap, 20, 20), (255, 0, 0, 255));
  // Pixel outside the backdrop-filter element should also remain the page background (red).
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));

  // Without a clip-path ancestor (so no Backdrop Root), the backdrop-filter should invert the red
  // backdrop to cyan.
  let pixmap = render(false, 60, 60);
  assert_eq!(pixel(&pixmap, 20, 20), (0, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 50, 50), (255, 0, 0, 255));
}
