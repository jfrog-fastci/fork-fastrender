use fastrender::paint::display_list::{
  BlendMode, BorderRadii, ClipItem, ClipShape, DisplayItem, DisplayList, FillRectItem,
  StackingContextItem,
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

fn ctx(bounds: Rect, transform_style: TransformStyle) -> StackingContextItem {
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
fn preserve_3d_projected_clip_masks_allow_reentrant_rendering() {
  // Regression test: projected clip masks for preserve-3d use a TLS scratch `RefCell`. The clip
  // override callback can render nested preserve-3d contexts (e.g. nested preserve-3d inside a
  // flattened plane), which re-enters the clip mask path and previously triggered a
  // `RefCell already borrowed` panic.
  let root_bounds = Rect::from_xywh(0.0, 0.0, 80.0, 80.0);
  let outer_clip_rect = Rect::from_xywh(0.0, 0.0, 40.0, 80.0);
  let inner_clip_rect = root_bounds;

  let mut list = DisplayList::new();

  // Outer preserve-3d scene with an active clip.
  list.push(DisplayItem::PushStackingContext(ctx(
    root_bounds,
    TransformStyle::Preserve3d,
  )));
  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Rect {
      rect: outer_clip_rect,
      radii: None,
    },
  }));

  // Flattened plane which itself contains another preserve-3d context that also has a clip.
  list.push(DisplayItem::PushStackingContext(ctx(root_bounds, TransformStyle::Flat)));
  list.push(DisplayItem::PushStackingContext(ctx(
    root_bounds,
    TransformStyle::Preserve3d,
  )));
  list.push(DisplayItem::PushClip(ClipItem {
    shape: ClipShape::Rect {
      rect: inner_clip_rect,
      radii: None,
    },
  }));
  list.push(DisplayItem::PushStackingContext(ctx(root_bounds, TransformStyle::Flat)));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: root_bounds,
    color: Rgba::RED,
  }));
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopClip);
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopStackingContext);

  list.push(DisplayItem::PopClip);
  list.push(DisplayItem::PopStackingContext);

  let pixmap = DisplayListRenderer::new(80, 80, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Outer clip should still be respected.
  assert_eq!(pixel(&pixmap, 10, 10), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 60, 10), (255, 255, 255, 255));
}

