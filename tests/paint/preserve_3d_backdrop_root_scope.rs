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
  preserve_root.has_backdrop_sensitive_descendants = true;
  preserve_root.has_clip_path = true;
  preserve_root.has_backdrop_sensitive_descendants = true;
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
  filtered_plane.has_backdrop_sensitive_descendants = true;
  filtered_plane.is_isolated = true;
  filtered_plane.establishes_backdrop_root = true;
  filtered_plane.has_backdrop_sensitive_descendants = true;
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

#[test]
fn preserve_3d_backdrop_filter_respects_intermediate_backdrop_root_scope() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let left_half = Rect::from_xywh(0.0, 0.0, 5.0, 10.0);

  let mut list = DisplayList::new();
  // Page background outside the preserve-3d context.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::RED,
  }));

  // Outer preserve-3d root that does *not* establish a backdrop root; pixels outside the nested
  // backdrop root should remain visible through transparent sampling regions.
  list.push(DisplayItem::PushStackingContext(context(bounds, TransformStyle::Preserve3d)));

  // Intermediate preserve-3d stacking context that establishes a Backdrop Root boundary (e.g. via
  // `will-change`). This boundary must scope descendant backdrop-filter sampling without forcing
  // the subtree to be flattened.
  let mut intermediate_root = context(bounds, TransformStyle::Preserve3d);
  intermediate_root.establishes_backdrop_root = true;
  intermediate_root.has_backdrop_sensitive_descendants = true;
  list.push(DisplayItem::PushStackingContext(intermediate_root));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: left_half,
    color: Rgba::GREEN,
  }));

  // Descendant plane with a backdrop-filter; content is empty/transparent so only the filtered
  // backdrop is visible.
  let mut filtered_plane = context(bounds, TransformStyle::Flat);
  filtered_plane.transform = Some(Transform3D::translate(0.0, 0.0, 10.0));
  filtered_plane.backdrop_filters = vec![ResolvedFilter::Invert(1.0)];
  filtered_plane.has_backdrop_sensitive_descendants = true;
  filtered_plane.is_isolated = true;
  filtered_plane.establishes_backdrop_root = true;
  list.push(DisplayItem::PushStackingContext(filtered_plane));
  list.push(DisplayItem::PopStackingContext);

  list.push(DisplayItem::PopStackingContext); // intermediate backdrop root
  list.push(DisplayItem::PopStackingContext); // outer preserve-3d root

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
    "expected inverted green (magenta) inside the nested preserve-3d backdrop-filter"
  );

  let right_px = pixmap.pixel(7, 5).expect("pixel in-bounds");
  assert_eq!(
    (right_px.red(), right_px.green(), right_px.blue(), right_px.alpha()),
    (255, 0, 0, 255),
    "expected backdrop-filter sampling to stop at the intermediate backdrop root, letting the page background show through"
  );
}

#[test]
fn preserve_3d_mix_blend_mode_respects_intermediate_backdrop_root_scope() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);
  let left_half = Rect::from_xywh(0.0, 0.0, 5.0, 10.0);

  let mut list = DisplayList::new();
  // Page background outside the preserve-3d context.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::RED,
  }));

  // Outer preserve-3d root that does *not* establish a backdrop root. The descendant blend-mode
  // operation must not see this red background once we cross the intermediate backdrop root
  // boundary below.
  list.push(DisplayItem::PushStackingContext(context(bounds, TransformStyle::Preserve3d)));

  // Intermediate preserve-3d stacking context that establishes a Backdrop Root boundary (e.g. via
  // `will-change`). Only paint green on the left half so the right half of the Backdrop Root Image
  // is transparent.
  let mut intermediate_root = context(bounds, TransformStyle::Preserve3d);
  intermediate_root.establishes_backdrop_root = true;
  intermediate_root.has_backdrop_sensitive_descendants = true;
  list.push(DisplayItem::PushStackingContext(intermediate_root));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: left_half,
    color: Rgba::GREEN,
  }));

  // Descendant flat plane with a mix-blend-mode. The blend backdrop should be scoped to the
  // intermediate backdrop root image (transparent on the right half), not the global canvas (red).
  let mut blended_plane = context(bounds, TransformStyle::Flat);
  blended_plane.transform = Some(Transform3D::translate(0.0, 0.0, 10.0));
  blended_plane.mix_blend_mode = BlendMode::Difference;
  list.push(DisplayItem::PushStackingContext(blended_plane));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: bounds,
    color: Rgba::BLUE,
  }));
  list.push(DisplayItem::PopStackingContext);

  list.push(DisplayItem::PopStackingContext); // intermediate backdrop root
  list.push(DisplayItem::PopStackingContext); // outer preserve-3d root

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
    (0, 255, 255, 255),
    "expected difference blend of blue over green to produce cyan"
  );

  let right_px = pixmap.pixel(7, 5).expect("pixel in-bounds");
  assert_eq!(
    (right_px.red(), right_px.green(), right_px.blue(), right_px.alpha()),
    (0, 0, 255, 255),
    "expected mix-blend-mode to stop at the intermediate backdrop root (transparent backdrop), so the blend does not see the red page background"
  );
}

#[test]
fn preserve_3d_root_backdrop_root_includes_canvas_background() {
  let bounds = Rect::from_xywh(0.0, 0.0, 10.0, 10.0);

  let mut list = DisplayList::new();

  // Preserve-3d paint root. Document roots establish a Backdrop Root boundary, but that boundary is
  // represented by the base canvas surface (i.e. we must not force an extra full-size transparent
  // offscreen layer just to mark it). If we did, a descendant `backdrop-filter` would incorrectly
  // sample an empty backdrop instead of the canvas background.
  let mut root = context(bounds, TransformStyle::Preserve3d);
  root.is_root = true;
  root.establishes_backdrop_root = true;
  root.has_backdrop_sensitive_descendants = true;
  list.push(DisplayItem::PushStackingContext(root));

  let mut filtered_plane = context(bounds, TransformStyle::Flat);
  filtered_plane.transform = Some(Transform3D::translate(0.0, 0.0, 10.0));
  filtered_plane.backdrop_filters = vec![ResolvedFilter::Invert(1.0)];
  filtered_plane.has_backdrop_sensitive_descendants = true;
  filtered_plane.is_isolated = true;
  filtered_plane.establishes_backdrop_root = true;
  list.push(DisplayItem::PushStackingContext(filtered_plane));
  list.push(DisplayItem::PopStackingContext);

  list.push(DisplayItem::PopStackingContext);

  let toggles = Arc::new(RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PRESERVE3D_DISABLE_SCENE".to_string(),
    "0".to_string(),
  )])));

  let pixmap = with_thread_runtime_toggles(toggles, || {
    DisplayListRenderer::new(10, 10, Rgba::RED, FontContext::new())
      .unwrap()
      .render(&list)
      .unwrap()
  });

  let px = pixmap.pixel(5, 5).expect("pixel in-bounds");
  assert_eq!(
    (px.red(), px.green(), px.blue(), px.alpha()),
    (0, 255, 255, 255),
    "expected invert backdrop-filter to sample the canvas background (red → cyan) even when the preserve-3d root establishes the document Backdrop Root"
  );
}
