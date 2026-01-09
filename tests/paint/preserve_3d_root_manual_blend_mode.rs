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
fn preserve_3d_root_manual_mix_blend_mode_is_applied_when_compositing_scene() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);

  let mut list = DisplayList::new();
  // Use a backdrop color whose HSV value is clearly not 1.0 so `HueHsv` has a visible effect.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::rgb(0, 128, 0),
  }));

  let mut root = stacking_context(bounds, TransformStyle::Preserve3d);
  root.mix_blend_mode = BlendMode::HueHsv;
  root.is_isolated = false;
  list.push(DisplayItem::PushStackingContext(root));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::RED,
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
  // HueHsv takes hue from src (red) and saturation/value from dest (dark green), yielding dark red.
  assert_eq!(pixel.alpha(), 255);
  assert!(
    pixel.red().abs_diff(128) <= 2 && pixel.green() <= 2 && pixel.blue() <= 2,
    "expected dark-red pixel, got rgba({}, {}, {}, {})",
    pixel.red(),
    pixel.green(),
    pixel.blue(),
    pixel.alpha()
  );
}

