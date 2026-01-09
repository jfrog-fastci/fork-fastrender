use super::util::bounding_box_for_color;
use fastrender::geometry::{Point, Rect};
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, ResolvedFilter, StackingContextItem,
  Transform3D,
};
use fastrender::paint::display_list_renderer::{DisplayListRenderer, PaintParallelism};
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::Rgba;

fn render(list: &DisplayList, width: u32, height: u32) -> tiny_skia::Pixmap {
  DisplayListRenderer::new(width, height, Rgba::TRANSPARENT, FontContext::new())
    .unwrap()
    .with_parallelism(PaintParallelism::disabled())
    .render(list)
    .unwrap()
}

#[test]
fn filter_drop_shadow_rotates_with_affine_transform() {
  // Filter Effects Level 1: filter operations are applied in the element's local coordinate system.
  // For an affine rotate, this means `drop-shadow(dx,dy)` rotates with the element.
  //
  // This test asserts that we do not accidentally interpret drop-shadow offsets in post-transform
  // (screen) space for affine-rendered stacking contexts.
  let mut list = DisplayList::new();

  let rect = Rect::from_xywh(20.0, 20.0, 10.0, 10.0);
  let cx = rect.x() + rect.width() * 0.5;
  let cy = rect.y() + rect.height() * 0.5;

  let transform = Transform3D::translate(cx, cy, 0.0)
    .multiply(&Transform3D::rotate_z(90.0_f32.to_radians()))
    .multiply(&Transform3D::translate(-cx, -cy, 0.0));

  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: false,
    has_backdrop_sensitive_descendants: false,
    bounds: rect,
    plane_rect: rect,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: Some(transform),
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: vec![ResolvedFilter::DropShadow {
      offset_x: 10.0,
      offset_y: 0.0,
      blur_radius: 0.0,
      spread: 0.0,
      color: Rgba::from_rgba8(0, 0, 0, 255),
    }],
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect,
    color: Rgba::from_rgba8(255, 0, 0, 255),
  }));
  list.push(DisplayItem::PopStackingContext);

  let pixmap = render(&list, 80, 80);

  // Correct behavior: with a +90deg rotation, the +X shadow offset becomes +Y (down).
  let shadow_down = pixmap.pixel(25, 35).unwrap();
  assert_eq!(
    (shadow_down.red(), shadow_down.green(), shadow_down.blue(), shadow_down.alpha()),
    (0, 0, 0, 255),
    "expected drop-shadow offset to rotate with element (shadow should be below)"
  );

  // The opposite pixel (right) must remain transparent.
  let shadow_right = pixmap.pixel(35, 25).unwrap();
  assert_eq!(
    shadow_right.alpha(),
    0,
    "expected no shadow on the right when offset rotates with the element"
  );
}

#[test]
fn filter_drop_shadow_projective_transform_uses_local_offset() {
  // Similar to the affine case above, but forces the projective-warp path.
  //
  // The key distinction vs. post-transform interpretation is that a local-space translation of the
  // source graphic is *not* equivalent to a constant screen-space translation under perspective.
  let mut list = DisplayList::new();

  let viewport = Rect::from_xywh(0.0, 0.0, 120.0, 100.0);
  let rect = Rect::from_xywh(20.0, 20.0, 60.0, 40.0);
  let cx = rect.x() + rect.width() * 0.5;
  let cy = rect.y() + rect.height() * 0.5;
  let dx = 60.0;

  let perspective = Transform3D::perspective(1000.0);
  let rotate = Transform3D::translate(cx, cy, 0.0)
    .multiply(&Transform3D::rotate_y(70.0_f32.to_radians()))
    .multiply(&Transform3D::translate(-cx, -cy, 0.0));

  // Full projective transform applied to the stacking context.
  let combined = perspective.multiply(&rotate);

  let center_screen = combined.project_point_2d(cx, cy).expect("center should project");
  let shadow_center_screen = combined
    .project_point_2d(cx + dx, cy)
    .expect("shadow center should project");
  let post_transform_shadow_center = Point::new(center_screen.x + dx, center_screen.y);

  assert!(
    (shadow_center_screen.x - post_transform_shadow_center.x).abs() > 5.0,
    "test setup expects perspective to change the effective screen-space offset (otherwise local/post offsets would coincide). projected={shadow_center_screen:?} post={post_transform_shadow_center:?}"
  );

  let to_px = |p: Point| -> (u32, u32) {
    let x = p.x.round() as i32;
    let y = p.y.round() as i32;
    assert!(
      x >= 0 && y >= 0,
      "pixel coordinates should be non-negative, got ({x},{y})"
    );
    (x as u32, y as u32)
  };

  let (shadow_x, shadow_y) = to_px(shadow_center_screen);
  let (post_x, post_y) = to_px(post_transform_shadow_center);

  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: false,
    bounds: viewport,
    plane_rect: viewport,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: None,
    child_perspective: Some(perspective),
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));
  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: false,
    bounds: rect,
    plane_rect: rect,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: false,
    transform: Some(rotate),
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: vec![ResolvedFilter::DropShadow {
      offset_x: dx,
      offset_y: 0.0,
      blur_radius: 0.0,
      spread: 0.0,
      // Use a vivid non-red shadow color so bbox detection doesn't accidentally match anti-aliased
      // red source pixels.
      color: Rgba::from_rgba8(0, 0, 255, 255),
    }],
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect,
    color: Rgba::from_rgba8(255, 0, 0, 255),
  }));
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopStackingContext);

  let pixmap = render(&list, 120, 100);

  assert!(
    shadow_x < pixmap.width() && shadow_y < pixmap.height(),
    "projected shadow pixel ({shadow_x},{shadow_y}) must be within the viewport"
  );
  assert!(
    post_x < pixmap.width() && post_y < pixmap.height(),
    "post-transform shadow pixel ({post_x},{post_y}) must be within the viewport"
  );

  let content_bbox = bounding_box_for_color(&pixmap, |(r, g, b, a)| {
    a > 0 && r > 0 && r >= g && r >= b
  })
  .expect("expected content to paint some pixels");

  let shadow_bbox = bounding_box_for_color(&pixmap, |(r, g, b, a)| {
    a > 0 && b > 50 && r < 40 && g < 40
  })
  .unwrap_or_else(|| {
    panic!(
      "expected drop-shadow to paint some blue pixels (but none found).\ncontent_bbox={content_bbox:?}\nprojected_local={shadow_center_screen:?} -> ({shadow_x},{shadow_y})\npost_transform={post_transform_shadow_center:?} -> ({post_x},{post_y})"
    )
  });

  // The projected local-space shadow center should land within the observed shadow bbox.
  let local_expected = (shadow_x, shadow_y);
  let local_inside = shadow_bbox.0 <= local_expected.0
    && local_expected.0 <= shadow_bbox.2
    && shadow_bbox.1 <= local_expected.1
    && local_expected.1 <= shadow_bbox.3;

  assert!(
    local_inside,
    "expected projected local-space shadow center to land inside shadow bbox.\nshadow_bbox={shadow_bbox:?}\ncontent_bbox={content_bbox:?}\nprojected_local={shadow_center_screen:?} -> {local_expected:?}\npost_transform={post_transform_shadow_center:?} -> ({post_x},{post_y})"
  );

  // The post-transform interpretation would put the shadow at a different screen-space offset.
  let post_expected = (post_x, post_y);
  let post_inside = shadow_bbox.0 <= post_expected.0
    && post_expected.0 <= shadow_bbox.2
    && shadow_bbox.1 <= post_expected.1
    && post_expected.1 <= shadow_bbox.3;
  assert!(
    !post_inside,
    "expected screen-space offset point to NOT land inside shadow bbox.\nshadow_bbox={shadow_bbox:?}\ncontent_bbox={content_bbox:?}\nprojected_local={shadow_center_screen:?} -> {local_expected:?}\npost_transform={post_transform_shadow_center:?} -> {post_expected:?}"
  );
}
