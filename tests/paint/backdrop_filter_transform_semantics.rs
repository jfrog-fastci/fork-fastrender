use fastrender::geometry::Rect;
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
fn backdrop_filter_drop_shadow_is_screen_aligned_under_affine_transform() {
  // Filter Effects Level 2 (backdrop-filter): the Backdrop Root Image is flattened into 2D
  // screen-space and then filtered before applying the inverse of the transforms between the
  // element and the Backdrop Root.
  //
  // This implies geometric filter parameters (e.g. drop-shadow offsets) are expressed in screen
  // coordinates, unlike `filter` (Filter Effects Level 1) which applies in the element's local
  // coordinate system.
  let mut list = DisplayList::new();

  // Backdrop content: a small opaque square on an otherwise transparent canvas.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(25.0, 25.0, 10.0, 10.0),
    color: Rgba::from_rgba8(255, 0, 0, 255),
  }));

  let bounds = Rect::from_xywh(20.0, 20.0, 40.0, 40.0);
  let cx = bounds.x() + bounds.width() * 0.5;
  let cy = bounds.y() + bounds.height() * 0.5;
  let transform = Transform3D::translate(cx, cy, 0.0)
    .multiply(&Transform3D::rotate_z(90.0_f32.to_radians()))
    .multiply(&Transform3D::translate(-cx, -cy, 0.0));

  list.push(DisplayItem::PushStackingContext(StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    is_root: false,
    establishes_backdrop_root: true,
    has_backdrop_sensitive_descendants: true,
    bounds,
    plane_rect: bounds,
    mix_blend_mode: BlendMode::Normal,
    opacity: 1.0,
    is_isolated: true,
    transform: Some(transform),
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: vec![ResolvedFilter::DropShadow {
      offset_x: 10.0,
      offset_y: 0.0,
      blur_radius: 0.0,
      spread: 0.0,
      color: Rgba::from_rgba8(0, 0, 255, 255),
    }],
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }));
  list.push(DisplayItem::PopStackingContext);

  let pixmap = render(&list, 80, 80);

  // With a +90deg rotation, a local-space offset would rotate +X into +Y (down). For backdrop
  // filters we expect screen-space semantics, so the shadow stays on the right.
  let shadow_right = pixmap.pixel(40, 30).unwrap();
  assert_eq!(
    (shadow_right.red(), shadow_right.green(), shadow_right.blue(), shadow_right.alpha()),
    (0, 0, 255, 255),
    "expected drop-shadow offset to remain screen-aligned (shadow should be to the right)"
  );

  let shadow_down = pixmap.pixel(30, 40).unwrap();
  assert_eq!(
    shadow_down.alpha(),
    0,
    "expected no shadow below when drop-shadow offset is screen-aligned"
  );
}
