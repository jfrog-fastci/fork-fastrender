use fastrender::geometry::Rect;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::style::position::Position;
use fastrender::style::types::BackfaceVisibility;
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
fn backface_visibility_hidden_wrapper_does_not_isolate_stacking_contexts() {
  // Root (white).
  //
  // - A: position: relative; backface-visibility: hidden; z-index: auto (must NOT create a
  //   stacking context).
  // - B: inside A, position: absolute; z-index: 10; paints red.
  // - C: sibling of A, position: relative; z-index: 5; paints blue.
  //
  // Expected: B should paint above C because it participates in the root stacking context; A must
  // not trap it below C.

  let mut b_style = ComputedStyle::default();
  b_style.position = Position::Absolute;
  b_style.z_index = Some(10);
  b_style.background_color = Rgba::RED;
  let b = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 30.0, 30.0),
    vec![],
    Arc::new(b_style),
  );

  let mut a_style = ComputedStyle::default();
  a_style.position = Position::Relative;
  a_style.backface_visibility = BackfaceVisibility::Hidden;
  let a = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
    vec![b],
    Arc::new(a_style),
  );

  let mut c_style = ComputedStyle::default();
  c_style.position = Position::Relative;
  c_style.z_index = Some(5);
  c_style.background_color = Rgba::BLUE;
  let c = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 50.0, 50.0),
    vec![],
    Arc::new(c_style),
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 60.0, 60.0), vec![a, c]);

  let list = DisplayListBuilder::new().build_with_stacking_tree(&root);
  let pixmap = DisplayListRenderer::new(60, 60, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap();

  // Overlap pixel should be red (B above C).
  assert_eq!(pixel(&pixmap, 10, 10), (255, 0, 0, 255));
  // Outside B but inside C should still be blue.
  assert_eq!(pixel(&pixmap, 40, 40), (0, 0, 255, 255));
  // Outside all content remains white.
  assert_eq!(pixel(&pixmap, 59, 59), (255, 255, 255, 255));
}

