use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, OpacityItem, StackingContextItem,
};
use fastrender::style::color::Rgba;
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::Rect;
use fastrender::paint::display_list_renderer::DisplayListRenderer;

fn stacking_context(bounds: Rect) -> StackingContextItem {
  StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds,
    plane_rect: bounds,
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
    has_clip_path: false,
  }
}

fn render(list: &DisplayList) -> tiny_skia::Pixmap {
  DisplayListRenderer::new(10, 10, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render(list)
    .expect("rendered")
}

#[test]
fn mix_blend_mode_samples_composited_backdrop_through_intermediate_layers() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let target = Rect::from_xywh(2.0, 2.0, 6.0, 6.0);

  let mut list = DisplayList::new();
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::GREEN,
  }));
  // `PushOpacity` creates an intermediate offscreen layer but is not a backdrop-root boundary.
  list.push(DisplayItem::PushOpacity(OpacityItem { opacity: 1.0 }));

  let mut blend = stacking_context(bounds);
  blend.mix_blend_mode = BlendMode::Difference;
  list.push(DisplayItem::PushStackingContext(blend));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: target,
    color: Rgba::RED,
  }));
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopOpacity);

  let pixmap = render(&list);
  let outside = pixmap.pixel(9, 9).expect("pixel in-bounds");
  assert_eq!(
    (outside.red(), outside.green(), outside.blue(), outside.alpha()),
    (0, 255, 0, 255),
    "expected background pixel to remain green"
  );

  let pixel = pixmap.pixel(5, 5).expect("pixel in-bounds");
  assert!(
    pixel.red() >= 240 && pixel.green() >= 240 && pixel.blue() <= 10 && pixel.alpha() >= 250,
    "expected yellow-ish output from `difference(red, green)` through intermediate layer, got rgba({}, {}, {}, {})",
    pixel.red(),
    pixel.green(),
    pixel.blue(),
    pixel.alpha()
  );
}

#[test]
fn mix_blend_mode_is_scoped_by_backdrop_root_boundary() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let target = Rect::from_xywh(2.0, 2.0, 6.0, 6.0);

  let mut list = DisplayList::new();
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::GREEN,
  }));

  // Backdrop root boundary that should prevent descendants from sampling the green backdrop.
  let mut root = stacking_context(bounds);
  root.establishes_backdrop_root = true;
  root.has_backdrop_sensitive_descendants = true;
  list.push(DisplayItem::PushStackingContext(root));
  list.push(DisplayItem::PushOpacity(OpacityItem { opacity: 1.0 }));

  let mut blend = stacking_context(bounds);
  blend.mix_blend_mode = BlendMode::Difference;
  list.push(DisplayItem::PushStackingContext(blend));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: target,
    color: Rgba::RED,
  }));
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopOpacity);
  list.push(DisplayItem::PopStackingContext);

  let pixmap = render(&list);
  let outside = pixmap.pixel(9, 9).expect("pixel in-bounds");
  assert_eq!(
    (outside.red(), outside.green(), outside.blue(), outside.alpha()),
    (0, 255, 0, 255),
    "expected background pixel to remain green"
  );

  let pixel = pixmap.pixel(5, 5).expect("pixel in-bounds");
  assert!(
    pixel.red() >= 240 && pixel.green() <= 10 && pixel.blue() <= 10 && pixel.alpha() >= 250,
    "expected red-ish output from `difference(red, transparent)` within backdrop root boundary, got rgba({}, {}, {}, {})",
    pixel.red(),
    pixel.green(),
    pixel.blue(),
    pixel.alpha()
  );
}
