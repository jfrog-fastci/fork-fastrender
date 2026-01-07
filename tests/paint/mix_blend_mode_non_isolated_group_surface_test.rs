use fastrender::paint::display_list::{
  BlendMode, BorderRadii, DisplayItem, DisplayList, FillRectItem, StackingContextItem,
};
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::types::{BackfaceVisibility, TransformStyle};
use fastrender::text::font_loader::FontContext;
use fastrender::{Rect, Rgba};

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let p = pixmap.pixel(x, y).unwrap();
  (p.red(), p.green(), p.blue(), p.alpha())
}

fn sc(bounds: Rect, mix_blend_mode: BlendMode, is_isolated: bool) -> StackingContextItem {
  StackingContextItem {
    z_index: 0,
    creates_stacking_context: true,
    establishes_backdrop_root: true,
    bounds,
    plane_rect: bounds,
    mix_blend_mode,
    opacity: 1.0,
    is_isolated,
    transform: None,
    child_perspective: None,
    transform_style: TransformStyle::Flat,
    backface_visibility: BackfaceVisibility::Visible,
    filters: Vec::new(),
    backdrop_filters: Vec::new(),
    radii: BorderRadii::ZERO,
    mask: None,
    has_clip_path: false,
  }
}

#[test]
fn non_isolated_blend_group_surface_sees_backdrop_pixels_for_descendant_blending() {
  // Regression for non-isolated group surface semantics:
  // - Outer group is a non-isolated mix-blend-mode stacking context (`is_isolated=false`).
  // - Descendant mix-blend-mode must blend against the already-painted backdrop (white).
  // - Isolated groups would instead blend against transparent.
  let bounds = Rect::from_xywh(0.0, 0.0, 16.0, 16.0);

  let render = |outer_isolated: bool| -> tiny_skia::Pixmap {
    let mut list = DisplayList::new();
    list.push(DisplayItem::PushStackingContext(sc(
      bounds,
      BlendMode::Multiply,
      outer_isolated,
    )));
    list.push(DisplayItem::PushStackingContext(sc(
      bounds,
      BlendMode::Difference,
      true,
    )));
    list.push(DisplayItem::FillRect(FillRectItem {
      rect: bounds,
      color: Rgba::RED,
    }));
    list.push(DisplayItem::PopStackingContext);
    list.push(DisplayItem::PopStackingContext);

    DisplayListRenderer::new(16, 16, Rgba::WHITE, FontContext::new())
      .expect("renderer")
      .render(&list)
      .expect("paint")
  };

  let isolated = render(true);
  let non_isolated = render(false);

  // Isolated => descendant blends against transparent => output stays red.
  assert_eq!(pixel(&isolated, 8, 8), (255, 0, 0, 255));
  // Non-isolated => descendant blends against white backdrop => difference(red, white) == cyan.
  assert_eq!(pixel(&non_isolated, 8, 8), (0, 255, 255, 255));
}

#[test]
fn nested_non_isolated_blend_groups_propagate_backdrop() {
  // Same as the previous test, but with an extra nested non-isolated group to ensure
  // initialization-from-backdrop composes correctly through multiple group surfaces.
  let outer_bounds = Rect::from_xywh(0.0, 0.0, 24.0, 24.0);
  let inner_bounds = Rect::from_xywh(12.0, 12.0, 12.0, 12.0);

  let mut list = DisplayList::new();
  list.push(DisplayItem::PushStackingContext(sc(
    outer_bounds,
    BlendMode::Multiply,
    false,
  )));

  // Paint something in the outer group that doesn't overlap the sampled pixel, ensuring the
  // backdrop path is exercised even when the group contains prior content.
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: Rect::from_xywh(0.0, 0.0, 6.0, 6.0),
    color: Rgba::GREEN,
  }));

  list.push(DisplayItem::PushStackingContext(sc(
    inner_bounds,
    BlendMode::Multiply,
    false,
  )));
  list.push(DisplayItem::PushStackingContext(sc(
    inner_bounds,
    BlendMode::Difference,
    true,
  )));
  list.push(DisplayItem::FillRect(FillRectItem {
    rect: inner_bounds,
    color: Rgba::RED,
  }));
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopStackingContext);
  list.push(DisplayItem::PopStackingContext);

  let pixmap = DisplayListRenderer::new(24, 24, Rgba::WHITE, FontContext::new())
    .expect("renderer")
    .render(&list)
    .expect("paint");

  assert_eq!(pixel(&pixmap, 18, 18), (0, 255, 255, 255));
}

