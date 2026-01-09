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
fn preserve_3d_inherited_clip_path_is_projected_with_transform() {
  let root_bounds = Rect::from_xywh(0.0, 0.0, 100.0, 100.0);

  let mut list = DisplayList::new();
  list.push(DisplayItem::PushStackingContext(ctx(
    root_bounds,
    TransformStyle::Preserve3d,
    None,
  )));

  // Wrapper introduces a non-identity transform that must be applied to inherited clips when
  // compositing preserve-3d planes.
  list.push(DisplayItem::PushStackingContext(ctx(
    root_bounds,
    TransformStyle::Preserve3d,
    Some(Transform3D::translate(40.0, 0.0, 0.0)),
  )));

  // A clip-path rectangle expressed as a polygon in the wrapper's local coordinate space.
  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Path {
      path: ResolvedClipPath::Polygon {
        points: vec![
          Point::new(0.0, 0.0),
          Point::new(50.0, 0.0),
          Point::new(50.0, 100.0),
          Point::new(0.0, 100.0),
        ],
        fill_rule: tiny_skia::FillRule::Winding,
      },
    },
  }));

  // Child plane paints a full-width rect; it should be clipped by the translated polygon.
  list.push(DisplayItem::PushStackingContext(ctx(
    root_bounds,
    TransformStyle::Flat,
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

  // The translated clip should cover x=40..90, so these points should both be visible.
  assert_eq!(pixel(&pixmap, 45, 50), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 70, 50), (255, 0, 0, 255));
}
