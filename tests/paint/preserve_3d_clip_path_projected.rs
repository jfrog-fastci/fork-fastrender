use fastrender::paint::clip_path::ResolvedClipPath;
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, ClipItem, ClipShape, DisplayItem, DisplayList, FillRectItem,
  StackingContextItem, Transform3D,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::{Point, Rect};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn ctx(
  bounds: Rect,
  transform_style: TransformStyle,
  transform: Option<Transform3D>,
  child_perspective: Option<Transform3D>,
) -> StackingContextItem {
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
    transform,
    child_perspective,
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
fn preserve_3d_projected_clip_path_polygon_clips_in_projected_space() {
  let root_bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
  let clip_rect = Rect::from_xywh(40.0, 30.0, 30.0, 30.0);

  let perspective = Transform3D::perspective(200.0);
  // Rotate around the global Y axis (through x=0), so the clip's center point projects outside the
  // original 2D bounds of the clip path.
  let rotate = Transform3D::rotate_y(70_f32.to_radians());
  let clip_transform = perspective.multiply(&rotate);

  let center = (
    clip_rect.x() + clip_rect.width() * 0.5,
    clip_rect.y() + clip_rect.height() * 0.5,
  );
  let (tx, ty, _tz, tw) = clip_transform.transform_point(center.0, center.1, 0.0);
  assert!(
    tw.is_finite() && tw.abs() >= Transform3D::MIN_PROJECTIVE_W && tw > 0.0,
    "expected projected w to be valid for test point, got w={tw}"
  );
  let px = tx / tw;
  let py = ty / tw;
  assert!(
    px < clip_rect.min_x() || px > clip_rect.max_x(),
    "expected projected x ({px}) to be outside unprojected clip rect x-range {}..{}",
    clip_rect.min_x(),
    clip_rect.max_x()
  );
  assert!(
    px >= 0.0 && py >= 0.0 && px < root_bounds.width() && py < root_bounds.height(),
    "expected projected point to land inside viewport; projected=({px},{py}) viewport=({}, {})",
    root_bounds.width(),
    root_bounds.height()
  );
  let sample_x = px.round().clamp(0.0, root_bounds.width() - 1.0) as u32;
  let sample_y = py.round().clamp(0.0, root_bounds.height() - 1.0) as u32;

  let mut list = DisplayList::new();
  list.push(DisplayItem::PushStackingContext(ctx(
    root_bounds,
    TransformStyle::Preserve3d,
    None,
    Some(perspective),
  )));
  list.push(DisplayItem::PushStackingContext(ctx(
    root_bounds,
    TransformStyle::Preserve3d,
    Some(rotate),
    None,
  )));

  let points = vec![
    Point::new(clip_rect.min_x(), clip_rect.min_y()),
    Point::new(clip_rect.max_x(), clip_rect.min_y()),
    Point::new(clip_rect.max_x(), clip_rect.max_y()),
    Point::new(clip_rect.min_x(), clip_rect.max_y()),
  ];
  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Path {
      path: ResolvedClipPath::Polygon {
        points,
        fill_rule: tiny_skia::FillRule::Winding,
      },
    },
  }));

  list.push(DisplayItem::PushStackingContext(ctx(
    root_bounds,
    TransformStyle::Flat,
    None,
    None,
  )));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: clip_rect,
    color: Rgba::RED,
  }));
  list.push(DisplayItem::PopStackingContext);

  list.push(DisplayItem::PopClip);
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopStackingContext);

  let pixmap = DisplayListRenderer::new(100, 100, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  assert_eq!(
    pixel(&pixmap, sample_x, sample_y),
    (255, 0, 0, 255),
    "expected projected clip-path to preserve interior pixel at ({sample_x},{sample_y})"
  );
}
