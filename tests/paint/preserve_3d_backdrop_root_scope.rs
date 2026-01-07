use fastrender::debug::runtime::{with_thread_runtime_toggles, RuntimeToggles};
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, ResolvedFilter, StackingContextItem,
  Transform3D,
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
    establishes_backdrop_root: false,
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
fn preserve_3d_backdrop_filter_respects_backdrop_root_scope() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let left_half = Rect::from_xywh(0.0, 0.0, 5.0, 10.0);

  let mut list = DisplayList::new();
  // Page background (outside the backdrop root boundary).
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::RED,
  }));

  // Preserve-3d root that also establishes a Filter Effects Level 2 Backdrop Root (e.g. via
  // `clip-path`). This requires `render_preserve_3d_context` to allocate a backdrop-root layer
  // (the normal item-by-item paint loop would do this automatically).
  //
  // Paint green only on the left half so the right half of the backdrop-root image remains
  // transparent. Descendant `backdrop-filter` must not sample the red page background through
  // this transparency.
  let mut preserve_root = context(bounds, TransformStyle::Preserve3d);
  preserve_root.establishes_backdrop_root = true;
  preserve_root.has_clip_path = true;
  list.push(DisplayItem::PushStackingContext(preserve_root));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: left_half,
    color: Rgba::GREEN,
  }));

  // Plane with a backdrop-filter; content is empty/transparent so only the filtered backdrop is
  // visible.
  let mut filtered_plane = context(bounds, TransformStyle::Flat);
  filtered_plane.transform = Some(Transform3D::translate(0.0, 0.0, 10.0));
  filtered_plane.backdrop_filters = vec![ResolvedFilter::Invert(1.0)];
  filtered_plane.is_isolated = true;
  filtered_plane.establishes_backdrop_root = true;
  list.push(DisplayItem::PushStackingContext(filtered_plane));
  list.push(DisplayItem::PopStackingContext);

  list.push(DisplayItem::PopStackingContext); // preserve-3d backdrop root

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

  let left_px = pixmap.pixel(2, 5).expect("pixel in-bounds");
  assert_eq!(
    (left_px.red(), left_px.green(), left_px.blue(), left_px.alpha()),
    (255, 0, 255, 255),
    "expected inverted green (magenta) inside the preserve-3d backdrop-filter"
  );

  let right_px = pixmap.pixel(7, 5).expect("pixel in-bounds");
  assert_eq!(
    (right_px.red(), right_px.green(), right_px.blue(), right_px.alpha()),
    (255, 0, 0, 255),
    "expected backdrop-filter to stop at the clip-path backdrop root (transparent backdrop), letting the page background show through"
  );
}
