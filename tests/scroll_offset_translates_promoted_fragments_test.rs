use fastrender::geometry::{Point, Rect};
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::scroll::ScrollState;
use fastrender::style::position::Position;
use fastrender::style::types::Overflow;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{ComputedStyle, Rgba};
use std::collections::HashMap;
use std::sync::Arc;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn render(promote_as_stacking_context: bool) -> tiny_skia::Pixmap {
  let mut scroller_style = ComputedStyle::default();
  scroller_style.overflow_x = Overflow::Hidden;
  scroller_style.overflow_y = Overflow::Scroll;
  let scroller_style = Arc::new(scroller_style);

  let mut red_style = ComputedStyle::default();
  red_style.background_color = Rgba::RED;
  let red_style = Arc::new(red_style);

  let mut blue_style = ComputedStyle::default();
  blue_style.background_color = Rgba::BLUE;
  let blue_style = Arc::new(blue_style);

  let red = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 4.0, 3.0),
    vec![],
    red_style,
  );
  let blue = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 3.0, 4.0, 3.0),
    vec![],
    blue_style,
  );

  let mut child_style = ComputedStyle::default();
  child_style.position = Position::Relative;
  if promote_as_stacking_context {
    child_style.z_index = Some(1);
  }
  let child_style = Arc::new(child_style);

  let child = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 4.0, 6.0),
    vec![red, blue],
    child_style,
  );

  let scroller = FragmentNode::new_with_style(
    Rect::from_xywh(2.0, 2.0, 4.0, 4.0),
    FragmentContent::Block { box_id: Some(1) },
    vec![child],
    scroller_style,
  );

  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![scroller]);

  let scroll_state = ScrollState::from_parts(
    Point::ZERO,
    HashMap::from([(1usize, Point::new(0.0, 2.0))]),
  );
  let list = DisplayListBuilder::new()
    .with_scroll_state(scroll_state)
    .build_with_stacking_tree(&root);

  DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap()
}

#[test]
fn element_scroll_offsets_translate_promoted_stacking_contexts() {
  let pixmap = render(true);

  // Outside the scroller remains white.
  assert_eq!(pixel(&pixmap, 1, 1), (255, 255, 255, 255));

  // The scroller is offset by (0, 2), shifting the child up by 2px. That means the blue stripe
  // should be visible at y=3.
  assert_eq!(pixel(&pixmap, 3, 3), (0, 0, 255, 255));
  assert_eq!(pixel(&pixmap, 3, 4), (0, 0, 255, 255));
}

#[test]
fn element_scroll_offsets_translate_promoted_positioned_descendants() {
  let pixmap = render(false);

  assert_eq!(pixel(&pixmap, 1, 1), (255, 255, 255, 255));
  assert_eq!(pixel(&pixmap, 3, 3), (0, 0, 255, 255));
  assert_eq!(pixel(&pixmap, 3, 4), (0, 0, 255, 255));
}

