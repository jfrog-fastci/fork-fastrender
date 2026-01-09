use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, ResolvedFilter,
  StackingContextItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::Rect;
use std::collections::HashMap;
use std::sync::Arc;

fn context(bounds: Rect, transform_style: TransformStyle) -> StackingContextItem {
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
fn preserve_3d_root_backdrop_filter_initializes_group_surface() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);

  let mut list = DisplayList::new();
  // Backdrop behind the preserve-3d root.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::GREEN,
  }));

  // Preserve-3d root with a backdrop-filter but no content: the filtered backdrop should still be
  // visible within the root bounds.
  let mut root = context(bounds, TransformStyle::Preserve3d);
  root.backdrop_filters = vec![ResolvedFilter::Invert(1.0)];
  root.is_isolated = true;
  list.push(DisplayItem::PushStackingContext(root));
  list.push(DisplayItem::PopStackingContext);

  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PRESERVE3D_DISABLE_SCENE".to_string(),
    "0".to_string(),
  )])));
  let pixmap = with_thread_runtime_toggles(toggles, || {
    DisplayListRenderer::new(10, 10, Rgba::TRANSPARENT, FontContext::new())
      .unwrap()
      .render(&list)
      .unwrap()
  });

  let px = pixmap.pixel(5, 5).expect("pixel in-bounds");
  assert_eq!(
    (px.red(), px.green(), px.blue(), px.alpha()),
    (255, 0, 255, 255),
    "expected invert backdrop-filter to affect the preserve-3d root backdrop (green → magenta)"
  );
}
