use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, StackingContextItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::Rect;
use std::collections::HashMap;
use std::sync::Arc;

fn stacking_context(bounds: Rect, transform_style: TransformStyle) -> StackingContextItem {
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
    transform_style,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }
}

#[test]
fn preserve_3d_root_opacity_applies_to_composited_scene() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);

  let mut list = DisplayList::new();
  let mut root = stacking_context(bounds, TransformStyle::Preserve3d);
  root.opacity = 0.5;
  list.push(DisplayItem::PushStackingContext(root));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::BLACK,
  }));
  list.push(DisplayItem::PopStackingContext);

  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PRESERVE3D_DISABLE_SCENE".to_string(),
    "0".to_string(),
  )])));

  let pixmap = with_thread_runtime_toggles(toggles, || {
    DisplayListRenderer::new(10, 10, Rgba::WHITE, FontContext::new())
      .unwrap()
      .render(&list)
      .unwrap()
  });

  let pixel = pixmap.pixel(5, 5).expect("pixel in-bounds");
  let expected = 128i16;
  let tolerance = 5i16;
  assert!(
    (pixel.red() as i16 - expected).abs() <= tolerance
      && (pixel.green() as i16 - expected).abs() <= tolerance
      && (pixel.blue() as i16 - expected).abs() <= tolerance,
    "expected roughly gray pixel, got rgba({}, {}, {}, {})",
    pixel.red(),
    pixel.green(),
    pixel.blue(),
    pixel.alpha()
  );
  assert_eq!(pixel.alpha(), 255);
}

