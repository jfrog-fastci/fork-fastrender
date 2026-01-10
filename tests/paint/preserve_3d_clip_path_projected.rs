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

fn assert_is_red(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 200 && g < 80 && b < 80 && a > 200,
    "{msg}: expected red pixel, got rgba({r},{g},{b},{a})"
  );
}

fn assert_is_white(rgba: (u8, u8, u8, u8), msg: &str) {
  let (r, g, b, a) = rgba;
  assert!(
    r > 200 && g > 200 && b > 200 && a > 200,
    "{msg}: expected white pixel, got rgba({r},{g},{b},{a})"
  );
}

fn project_to_pixel(transform: &Transform3D, x: f32, y: f32, viewport: Rect) -> (u32, u32, f32, f32) {
  let (tx, ty, _tz, tw) = transform.transform_point(x, y, 0.0);
  assert!(
    tw.is_finite() && tw.abs() >= Transform3D::MIN_PROJECTIVE_W && tw > 0.0,
    "expected projected w to be valid for test point, got w={tw}"
  );
  let px = tx / tw;
  let py = ty / tw;
  assert!(
    px >= 0.0 && py >= 0.0 && px < viewport.width() && py < viewport.height(),
    "expected projected point to land inside viewport; projected=({px},{py}) viewport=({}, {})",
    viewport.width(),
    viewport.height()
  );
  let sample_x = px.round().clamp(0.0, viewport.width() - 1.0) as u32;
  let sample_y = py.round().clamp(0.0, viewport.height() - 1.0) as u32;
  (sample_x, sample_y, px, py)
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
    mask_border: None,
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
  let (sample_x, sample_y, px, _py) = project_to_pixel(&clip_transform, center.0, center.1, root_bounds);
  assert!(
    px < clip_rect.min_x() || px > clip_rect.max_x(),
    "expected projected x ({px}) to be outside unprojected clip rect x-range {}..{}",
    clip_rect.min_x(),
    clip_rect.max_x()
  );

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

  assert_is_red(
    pixel(&pixmap, sample_x, sample_y),
    &format!("expected projected clip-path to preserve interior pixel at ({sample_x},{sample_y})"),
  );
}

#[test]
fn preserve_3d_projected_clip_path_circle_clips_in_projected_space() {
  let root_bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
  let clip_rect = Rect::from_xywh(40.0, 30.0, 30.0, 30.0);

  let perspective = Transform3D::perspective(200.0);
  let rotate = Transform3D::rotate_y(70_f32.to_radians());
  let clip_transform = perspective.multiply(&rotate);

  let center = (
    clip_rect.x() + clip_rect.width() * 0.5,
    clip_rect.y() + clip_rect.height() * 0.5,
  );
  let (center_x, center_y, px, _py) = project_to_pixel(&clip_transform, center.0, center.1, root_bounds);
  assert!(
    px < clip_rect.min_x() || px > clip_rect.max_x(),
    "expected projected x ({px}) to be outside unprojected clip rect x-range {}..{}",
    clip_rect.min_x(),
    clip_rect.max_x()
  );

  let outside = (clip_rect.min_x() + 5.0, clip_rect.max_y() - 1.0);
  let (outside_x, outside_y, _opx, _opy) =
    project_to_pixel(&clip_transform, outside.0, outside.1, root_bounds);

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

  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Path {
      path: ResolvedClipPath::Circle {
        center: Point::new(center.0, center.1),
        radius: clip_rect.width() * 0.5,
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

  assert_is_red(
    pixel(&pixmap, center_x, center_y),
    &format!("expected projected circle clip-path to preserve center pixel at ({center_x},{center_y})"),
  );
  assert_is_white(
    pixel(&pixmap, outside_x, outside_y),
    &format!(
      "expected projected circle clip-path to clip outside pixel at ({outside_x},{outside_y})"
    ),
  );
}

#[test]
fn preserve_3d_projected_clip_path_ellipse_clips_in_projected_space() {
  let root_bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
  let clip_rect = Rect::from_xywh(40.0, 30.0, 30.0, 30.0);

  let perspective = Transform3D::perspective(200.0);
  let rotate = Transform3D::rotate_y(70_f32.to_radians());
  let clip_transform = perspective.multiply(&rotate);

  let center = (
    clip_rect.x() + clip_rect.width() * 0.5,
    clip_rect.y() + clip_rect.height() * 0.5,
  );
  let (center_x, center_y, px, _py) = project_to_pixel(&clip_transform, center.0, center.1, root_bounds);
  assert!(
    px < clip_rect.min_x() || px > clip_rect.max_x(),
    "expected projected x ({px}) to be outside unprojected clip rect x-range {}..{}",
    clip_rect.min_x(),
    clip_rect.max_x()
  );

  let outside = (clip_rect.min_x() + 5.0, clip_rect.max_y() - 1.0);
  let (outside_x, outside_y, _opx, _opy) =
    project_to_pixel(&clip_transform, outside.0, outside.1, root_bounds);

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

  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Path {
      path: ResolvedClipPath::Ellipse {
        center: Point::new(center.0, center.1),
        radius_x: clip_rect.width() * 0.5,
        radius_y: clip_rect.height() * 0.4,
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

  assert_is_red(
    pixel(&pixmap, center_x, center_y),
    &format!(
      "expected projected ellipse clip-path to preserve center pixel at ({center_x},{center_y})"
    ),
  );
  assert_is_white(
    pixel(&pixmap, outside_x, outside_y),
    &format!(
      "expected projected ellipse clip-path to clip outside pixel at ({outside_x},{outside_y})"
    ),
  );
}

#[test]
fn preserve_3d_projected_clip_path_inset_round_clips_in_projected_space() {
  let root_bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
  let clip_rect = Rect::from_xywh(40.0, 30.0, 30.0, 30.0);

  let perspective = Transform3D::perspective(200.0);
  let rotate = Transform3D::rotate_y(70_f32.to_radians());
  let clip_transform = perspective.multiply(&rotate);

  let center = (
    clip_rect.x() + clip_rect.width() * 0.5,
    clip_rect.y() + clip_rect.height() * 0.5,
  );
  let (center_x, center_y, px, _py) = project_to_pixel(&clip_transform, center.0, center.1, root_bounds);
  assert!(
    px < clip_rect.min_x() || px > clip_rect.max_x(),
    "expected projected x ({px}) to be outside unprojected clip rect x-range {}..{}",
    clip_rect.min_x(),
    clip_rect.max_x()
  );

  // A point inside the inset rect but outside the rounded corner.
  let outside_corner = (clip_rect.min_x() + 2.0, clip_rect.max_y() - 1.0);
  let (corner_x, corner_y, _cpx, _cpy) =
    project_to_pixel(&clip_transform, outside_corner.0, outside_corner.1, root_bounds);

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

  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Path {
      path: ResolvedClipPath::Inset {
        rect: clip_rect,
        radii: BorderRadii::uniform(10.0),
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

  assert_is_red(
    pixel(&pixmap, center_x, center_y),
    &format!(
      "expected projected inset(round) clip-path to preserve center pixel at ({center_x},{center_y})"
    ),
  );
  assert_is_white(
    pixel(&pixmap, corner_x, corner_y),
    &format!(
      "expected projected inset(round) clip-path to clip corner pixel at ({corner_x},{corner_y})"
    ),
  );
}

#[test]
fn preserve_3d_projected_clip_path_respects_evenodd_fill_rule() {
  let root_bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
  let outer = Rect::from_xywh(40.0, 20.0, 40.0, 60.0);
  let hole = Rect::from_xywh(50.0, 35.0, 10.0, 10.0);

  let perspective = Transform3D::perspective(200.0);
  // Ensure the clip is truly projective so the renderer must use the preserve-3d clip mask warp
  // path (rather than affine canvas clipping).
  let rotate = Transform3D::rotate_y(70_f32.to_radians());
  let clip_transform = perspective.multiply(&rotate);

  let project = |x: f32, y: f32| -> (f32, f32, u32, u32) {
    let (tx, ty, _tz, tw) = clip_transform.transform_point(x, y, 0.0);
    assert!(
      tw.is_finite() && tw.abs() >= Transform3D::MIN_PROJECTIVE_W && tw > 0.0,
      "expected projected w to be valid for test point, got w={tw}"
    );
    let px = tx / tw;
    let py = ty / tw;
    assert!(
      px >= 0.0 && py >= 0.0 && px < root_bounds.width() && py < root_bounds.height(),
      "expected projected point to land inside viewport; projected=({px},{py}) viewport=({}, {})",
      root_bounds.width(),
      root_bounds.height()
    );
    let sample_x = px.round().clamp(0.0, root_bounds.width() - 1.0) as u32;
    let sample_y = py.round().clamp(0.0, root_bounds.height() - 1.0) as u32;
    (px, py, sample_x, sample_y)
  };

  // Point inside the outer clip region but outside the hole.
  let (outer_px, _outer_py, outer_x, outer_y) = project(70.0, 50.0);
  assert!(
    outer_px < outer.min_x() || outer_px > outer.max_x(),
    "expected projected outer sample x ({outer_px}) to be outside unprojected clip rect x-range {}..{}",
    outer.min_x(),
    outer.max_x()
  );

  // Point inside the hole. If the fill rule is ignored (treated as winding), this pixel would be
  // painted red.
  let hole_center = (hole.x() + hole.width() * 0.5, hole.y() + hole.height() * 0.5);
  let (_hole_px, _hole_py, hole_x, hole_y) = project(hole_center.0, hole_center.1);

  let mut builder = tiny_skia::PathBuilder::new();
  builder.move_to(outer.min_x(), outer.min_y());
  builder.line_to(outer.max_x(), outer.min_y());
  builder.line_to(outer.max_x(), outer.max_y());
  builder.line_to(outer.min_x(), outer.max_y());
  builder.close();
  // Same winding direction as the outer rect; only even-odd should punch a hole.
  builder.move_to(hole.min_x(), hole.min_y());
  builder.line_to(hole.max_x(), hole.min_y());
  builder.line_to(hole.max_x(), hole.max_y());
  builder.line_to(hole.min_x(), hole.max_y());
  builder.close();
  let path = builder.finish().expect("valid clip-path");

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
  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Path {
      path: ResolvedClipPath::Path {
        path,
        fill_rule: tiny_skia::FillRule::EvenOdd,
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
    rect: root_bounds,
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
    pixel(&pixmap, outer_x, outer_y),
    (255, 0, 0, 255),
    "expected projected clip-path to preserve interior pixel at ({outer_x},{outer_y})"
  );
  assert_eq!(
    pixel(&pixmap, hole_x, hole_y),
    (255, 255, 255, 255),
    "expected even-odd clip-path hole pixel at ({hole_x},{hole_y}) to remain background-colored"
  );
}
