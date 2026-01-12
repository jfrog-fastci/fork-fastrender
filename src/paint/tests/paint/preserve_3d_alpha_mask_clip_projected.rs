use crate::paint::display_list::{
  BlendMode, BorderRadii, ClipItem, ClipShape, DisplayItem, DisplayList, FillRectItem, ImageData,
  StackingContextItem, Transform3D,
};
use crate::paint::display_list_renderer::DisplayListRenderer;
use crate::style::color::Rgba;
use crate::style::types::{BackfaceVisibility, TransformStyle};
use crate::text::font_loader::FontContext;
use crate::Rect;
use std::sync::Arc;

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

fn project_to_pixel(
  transform: &Transform3D,
  x: f32,
  y: f32,
  viewport: Rect,
) -> (u32, u32, f32, f32) {
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
fn preserve_3d_projected_alpha_mask_clip_clips_in_projected_space() {
  let root_bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);
  let clip_rect = Rect::from_xywh(40.0, 30.0, 30.0, 30.0);

  let perspective = Transform3D::perspective(200.0);
  // Rotate around the global Y axis (through x=0), so the clip's center point projects outside the
  // original 2D bounds of the clip mask region.
  let rotate = Transform3D::rotate_y(70_f32.to_radians());
  let clip_transform = perspective.multiply(&rotate);

  let center = (
    clip_rect.x() + clip_rect.width() * 0.5,
    clip_rect.y() + clip_rect.height() * 0.5,
  );
  let (center_x, center_y, px, _py) =
    project_to_pixel(&clip_transform, center.0, center.1, root_bounds);
  assert!(
    px < clip_rect.min_x() || px > clip_rect.max_x(),
    "expected projected x ({px}) to be outside unprojected clip rect x-range {}..{}",
    clip_rect.min_x(),
    clip_rect.max_x()
  );

  let outside = (10.0, 50.0);
  let (outside_x, outside_y, _opx, _opy) =
    project_to_pixel(&clip_transform, outside.0, outside.1, root_bounds);

  let mut pixels = Vec::with_capacity((100 * 100 * 4) as usize);
  for y in 0..100u32 {
    for x in 0..100u32 {
      let inside = (x as f32) >= clip_rect.min_x()
        && (x as f32) < clip_rect.max_x()
        && (y as f32) >= clip_rect.min_y()
        && (y as f32) < clip_rect.max_y();
      let a = if inside { 255 } else { 0 };
      pixels.extend_from_slice(&[0, 0, 0, a]);
    }
  }
  let mask_image = Arc::new(ImageData::new_pixels(100, 100, pixels));

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
    shape: ClipShape::AlphaMask {
      image: mask_image,
      rect: root_bounds,
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

  assert_is_red(
    pixel(&pixmap, center_x, center_y),
    &format!(
      "expected projected alpha-mask clip to preserve interior pixel at ({center_x},{center_y})"
    ),
  );
  assert_is_white(
    pixel(&pixmap, outside_x, outside_y),
    &format!(
      "expected projected alpha-mask clip to clip outside pixel at ({outside_x},{outside_y})"
    ),
  );
}
