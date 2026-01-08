use fastrender::geometry::{Point, Rect};
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::DisplayListRenderer;
use fastrender::scroll::ScrollState;
use fastrender::style::position::Position;
use fastrender::text::font_loader::FontContext;
use fastrender::tree::fragment_tree::FragmentNode;
use fastrender::{ComputedStyle, Rgba};
use std::sync::Arc;

fn pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("pixel inside viewport");
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn render(root: &FragmentNode, scroll: Point) -> tiny_skia::Pixmap {
  let scroll_state = ScrollState::with_viewport(scroll);
  let offset = Point::new(-scroll.x, -scroll.y);

  let list = DisplayListBuilder::new()
    .with_scroll_state(scroll_state)
    .build_with_stacking_tree_offset(root, offset);

  DisplayListRenderer::new(8, 8, Rgba::WHITE, FontContext::new())
    .unwrap()
    .render(&list)
    .unwrap()
}

fn base_scene_with_fixed_root() -> FragmentNode {
  let mut green_style = ComputedStyle::default();
  green_style.background_color = Rgba::GREEN;
  let green_style = Arc::new(green_style);

  let mut blue_style = ComputedStyle::default();
  blue_style.background_color = Rgba::BLUE;
  let blue_style = Arc::new(blue_style);

  let mut red_fixed_style = ComputedStyle::default();
  red_fixed_style.background_color = Rgba::RED;
  red_fixed_style.position = Position::Fixed;
  let red_fixed_style = Arc::new(red_fixed_style);

  let stripe_a = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 8.0, 2.0),
    vec![],
    green_style,
  );
  let stripe_b = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
    vec![],
    blue_style,
  );
  let fixed = FragmentNode::new_block_styled(
    // Cover the left half so we can check both covered/uncovered pixels.
    Rect::from_xywh(0.0, 0.0, 4.0, 2.0),
    vec![],
    red_fixed_style,
  );

  // Place the fixed element last so it paints over the scrolled content.
  FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![stripe_a, stripe_b, fixed])
}

fn scene_with_fixed_containing_block() -> FragmentNode {
  let mut container_style = ComputedStyle::default();
  container_style.containment.layout = true;
  let container_style = Arc::new(container_style);

  let mut green_style = ComputedStyle::default();
  green_style.background_color = Rgba::GREEN;
  let green_style = Arc::new(green_style);

  let mut blue_style = ComputedStyle::default();
  blue_style.background_color = Rgba::BLUE;
  let blue_style = Arc::new(blue_style);

  let mut red_fixed_style = ComputedStyle::default();
  red_fixed_style.background_color = Rgba::RED;
  red_fixed_style.position = Position::Fixed;
  let red_fixed_style = Arc::new(red_fixed_style);

  let stripe_a = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 8.0, 2.0),
    vec![],
    green_style,
  );
  let stripe_b = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
    vec![],
    blue_style,
  );
  let fixed = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 4.0, 2.0),
    vec![],
    red_fixed_style,
  );

  let container = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
    vec![stripe_a, stripe_b, fixed],
    container_style,
  );

  FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 8.0, 8.0), vec![container])
}

fn scene_with_nested_fixed_elements() -> FragmentNode {
  let mut blue_style = ComputedStyle::default();
  blue_style.background_color = Rgba::BLUE;
  let blue_style = Arc::new(blue_style);

  let mut black_style = ComputedStyle::default();
  black_style.background_color = Rgba::BLACK;
  let black_style = Arc::new(black_style);

  let mut outer_fixed_style = ComputedStyle::default();
  outer_fixed_style.background_color = Rgba::RED;
  outer_fixed_style.position = Position::Fixed;
  let outer_fixed_style = Arc::new(outer_fixed_style);

  let mut inner_fixed_style = ComputedStyle::default();
  inner_fixed_style.background_color = Rgba::GREEN;
  inner_fixed_style.position = Position::Fixed;
  let inner_fixed_style = Arc::new(inner_fixed_style);

  let stripe_a = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 8.0, 2.0),
    vec![],
    blue_style,
  );
  let stripe_b = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
    vec![],
    black_style,
  );

  // Nested fixed elements remain positioned relative to the viewport. Model the nested fixed
  // element by giving it an origin relative to the outer fixed element such that its absolute
  // position is still (0, 0) at scroll=0.
  let inner_fixed = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, -2.0, 8.0, 2.0),
    vec![],
    inner_fixed_style,
  );
  let outer_fixed = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 2.0, 8.0, 2.0),
    vec![inner_fixed],
    outer_fixed_style,
  );

  // Place the outer fixed element last so it paints over the scrolling content.
  FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 8.0, 8.0),
    vec![stripe_a, stripe_b, outer_fixed],
  )
}

#[test]
fn fixed_position_is_not_translated_by_viewport_scroll() {
  let root = base_scene_with_fixed_root();
  let pixmap = render(&root, Point::new(0.0, 2.0));

  // Fixed element stays pinned to the viewport.
  assert_eq!(pixel(&pixmap, 1, 0), (255, 0, 0, 255));
  assert_eq!(pixel(&pixmap, 1, 1), (255, 0, 0, 255));

  // Content scrolls underneath it: the second (blue) stripe moves from y=2..4 to y=0..2.
  assert_eq!(pixel(&pixmap, 6, 0), (0, 0, 255, 255));
  assert_eq!(pixel(&pixmap, 6, 1), (0, 0, 255, 255));
}

#[test]
fn fixed_position_inside_fixed_containing_block_is_translated_by_viewport_scroll() {
  let root = scene_with_fixed_containing_block();
  let pixmap = render(&root, Point::new(0.0, 2.0));

  // The container establishes the fixed containing block, so the fixed element scrolls away.
  assert_eq!(pixel(&pixmap, 1, 0), (0, 0, 255, 255));
  assert_eq!(pixel(&pixmap, 1, 1), (0, 0, 255, 255));
}

#[test]
fn nested_fixed_position_is_not_double_translated_by_viewport_scroll() {
  let root = scene_with_nested_fixed_elements();
  let pixmap = render(&root, Point::new(0.0, 2.0));

  // Inner fixed stays pinned at y=0 and does not cancel scroll twice.
  assert_eq!(pixel(&pixmap, 1, 0), (0, 255, 0, 255));
  // Outer fixed stays pinned at y=2.
  assert_eq!(pixel(&pixmap, 1, 2), (255, 0, 0, 255));
}
