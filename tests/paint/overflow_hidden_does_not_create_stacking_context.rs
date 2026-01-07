use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::position::Position;
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
fn overflow_hidden_wrapper_does_not_isolate_stacking_contexts() {
  // Root (white).

  // Sibling B: positioned element with z-index: 1 paints blue.
  let mut sibling_b_style = ComputedStyle::default();
  sibling_b_style.position = Position::Relative;
  sibling_b_style.z_index = Some(1);
  sibling_b_style.background_color = Rgba::BLUE;
  let sibling_b_style = Arc::new(sibling_b_style);
  let sibling_b = FragmentNode::new_block_styled(
    Rect::from_xywh(10.0, 10.0, 20.0, 20.0),
    vec![],
    sibling_b_style,
  );

  // Child C: positioned element with z-index: 2 paints red, overlapping sibling B.
  let mut child_c_style = ComputedStyle::default();
  child_c_style.position = Position::Absolute;
  child_c_style.z_index = Some(2);
  child_c_style.background_color = Rgba::RED;
  let child_c_style = Arc::new(child_c_style);
  let child_c =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 20.0, 20.0), vec![], child_c_style);

  // Sibling A: position: relative; overflow: hidden; z-index: auto. This must NOT create a
  // stacking context, otherwise child C is trapped below sibling B.
  let mut sibling_a_style = ComputedStyle::default();
  sibling_a_style.position = Position::Relative;
  sibling_a_style.overflow_x = Overflow::Hidden;
  sibling_a_style.overflow_y = Overflow::Hidden;
  let sibling_a_style = Arc::new(sibling_a_style);
  let sibling_a = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 20.0, 20.0),
    vec![child_c],
    sibling_a_style,
  );

  let root =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 30.0, 30.0), vec![sibling_a, sibling_b]);

  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
  let pixmap = DisplayListRenderer::new(30, 30, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Overlap pixel should be red (child C above sibling B).
  assert_eq!(pixel(&pixmap, 15, 15), (255, 0, 0, 255));

  // Non-overlapping pixels still paint correctly.
  assert_eq!(pixel(&pixmap, 25, 25), (0, 0, 255, 255));
  assert_eq!(pixel(&pixmap, 29, 0), (255, 255, 255, 255));
}

