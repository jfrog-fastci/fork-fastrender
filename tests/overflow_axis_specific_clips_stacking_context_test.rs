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

fn render(parent_overflow_x: Overflow, parent_overflow_y: Overflow) -> tiny_skia::Pixmap {
  let mut parent_style = ComputedStyle::default();
  parent_style.overflow_x = parent_overflow_x;
  parent_style.overflow_y = parent_overflow_y;
  let parent_style = Arc::new(parent_style);

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Relative;
  child_style.z_index = Some(1);
  child_style.background_color = Rgba::RED;
  let child_style = Arc::new(child_style);

  let child =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 6.0, 6.0), vec![], child_style);

  let mut parent = FragmentNode::new_block_styled(
    Rect::from_xywh(2.0, 2.0, 4.0, 4.0),
    vec![child],
    parent_style,
  );
  // Expand scrollable overflow to include the promoted child stacking context. The clip-chain
  // implementation uses this to compute axis-specific overflow clips (visible/clip).
  parent.scroll_overflow = Rect::from_xywh(0.0, 0.0, 6.0, 6.0);

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![parent]);

  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
  DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap()
}

#[test]
fn overflow_x_visible_y_clip_clips_only_y_for_promoted_stacking_context() {
  let pixmap = render(Overflow::Visible, Overflow::Clip);

  assert_eq!(pixel(&pixmap, 1, 1), (255, 255, 255, 255));
  // Inside the parent (and child) paints red.
  assert_eq!(pixel(&pixmap, 3, 3), (255, 0, 0, 255));
  // Horizontal overflow remains visible.
  assert_eq!(pixel(&pixmap, 7, 4), (255, 0, 0, 255));
  // Vertical overflow is clipped.
  assert_eq!(pixel(&pixmap, 4, 7), (255, 255, 255, 255));
}

#[test]
fn overflow_x_clip_y_visible_clips_only_x_for_promoted_stacking_context() {
  let pixmap = render(Overflow::Clip, Overflow::Visible);

  assert_eq!(pixel(&pixmap, 1, 1), (255, 255, 255, 255));
  // Inside the parent (and child) paints red.
  assert_eq!(pixel(&pixmap, 3, 3), (255, 0, 0, 255));
  // Horizontal overflow is clipped.
  assert_eq!(pixel(&pixmap, 7, 4), (255, 255, 255, 255));
  // Vertical overflow remains visible.
  assert_eq!(pixel(&pixmap, 4, 7), (255, 0, 0, 255));
}

