use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::types::Overflow;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::ComputedStyle;
use fastrender::Rgba;
use std::sync::Arc;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

#[test]
fn overflow_hidden_with_zero_sized_padding_box_clips_descendants() {
  let mut parent_style = ComputedStyle::default();
  parent_style.overflow_x = Overflow::Hidden;
  parent_style.overflow_y = Overflow::Hidden;
  let parent_style = Arc::new(parent_style);

  let mut child_style = ComputedStyle::default();
  child_style.background_color = Rgba::RED;
  let child_style = Arc::new(child_style);

  let child =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 2.0, 2.0), vec![], child_style);

  // Parent has a zero-sized padding box, so overflow clipping should suppress all descendant
  // painting.
  let parent = FragmentNode::new_block_styled(
    Rect::from_xywh(1.0, 1.0, 0.0, 0.0),
    vec![child],
    parent_style,
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 4.0, 4.0), vec![parent]);

  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
  let pixmap = DisplayListRenderer::new(4, 4, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  assert_eq!(pixel(&pixmap, 2, 2), (255, 255, 255, 255));
}

