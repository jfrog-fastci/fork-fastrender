use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::position::Position;
use fastrender::style::types::Overflow;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{ComputedStyle, Rgba};
use std::sync::Arc;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn overflow_hidden_clips_stacking_context_children() {
  // Regression coverage for the clip chain used by the stacking-context builder.
  //
  // `overflow: hidden` does not create a stacking context, but it does establish a clipping scope
  // for descendants. When a descendant creates a stacking context, it is promoted to the nearest
  // ancestor stacking context during paint ordering; the clip must still apply.

  let mut parent_style = ComputedStyle::default();
  parent_style.overflow_x = Overflow::Hidden;
  parent_style.overflow_y = Overflow::Hidden;
  let parent_style = Arc::new(parent_style);

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Relative;
  child_style.z_index = Some(1);
  child_style.background_color = Rgba::RED;
  let child_style = Arc::new(child_style);

  // Child extends outside the parent, but should be clipped by overflow:hidden on the parent.
  let child =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 6.0, 4.0), vec![], child_style);

  let parent = FragmentNode::new_block_styled(
    Rect::from_xywh(2.0, 2.0, 4.0, 4.0),
    vec![child],
    parent_style,
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![parent]);

  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
  let pixmap = DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Pixel inside the child but outside the parent's overflow clip stays white.
  assert_eq!(pixel(&pixmap, 7, 3), (255, 255, 255, 255));
  // Pixel inside both parent+child paints red.
  assert_eq!(pixel(&pixmap, 3, 3), (255, 0, 0, 255));
}

