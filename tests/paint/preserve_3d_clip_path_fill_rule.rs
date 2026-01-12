use fastrender::paint::clip_path::ResolvedClipPath;
use fastrender::paint::display_list::{
  BlendMode, BorderRadii, ClipItem, ClipShape, DisplayItem, DisplayList, FillRectItem,
  StackingContextItem, Transform3D,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::color::Rgba;
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::Rect;

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
    mask_border: None,
    has_clip_path: false,
  }
}

#[test]
fn preserve_3d_inherited_clip_path_respects_fill_rule() {
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

  // A clip-path path containing a hole (inner rect) that relies on even-odd fill.
  //
  // With FillRule::Winding, the inner rect is not a hole here because it uses the same winding
  // direction as the outer rect.
  let mut builder = tiny_skia::PathBuilder::new();
  builder.move_to(0.0, 0.0);
  builder.line_to(50.0, 0.0);
  builder.line_to(50.0, 100.0);
  builder.line_to(0.0, 100.0);
  builder.close();
  builder.move_to(10.0, 40.0);
  builder.line_to(40.0, 40.0);
  builder.line_to(40.0, 60.0);
  builder.line_to(10.0, 60.0);
  builder.close();
  let path = builder.finish().expect("valid clip-path");

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

  // Outer clip should cover x=40..90, so this point should be visible (red). This also ensures
  // we didn't fall back to affine canvas clipping (which would ignore the wrapper transform).
  assert_eq!(pixel(&pixmap, 70, 10), (255, 0, 0, 255));

  // This point is inside the translated clip, but also inside the (translated) hole, so it must
  // remain background-colored.
  assert_eq!(pixel(&pixmap, 60, 50), (255, 255, 255, 255));
}
