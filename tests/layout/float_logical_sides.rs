use std::sync::Arc;

use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::float::{Clear, Float};
use fastrender::style::types::{Direction, WritingMode};
use fastrender::style::values::Length;
use fastrender::{BoxNode, BoxTree, ComputedStyle, FormattingContext, FragmentNode};

const EPS: f32 = 0.1;

fn block_style() -> ComputedStyle {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style
}

fn float_block(float: Float) -> BoxNode {
  let mut style = block_style();
  style.float = float;
  style.clear = Clear::Both;
  style.width = Some(Length::px(50.0));
  style.width_keyword = None;
  style.height = Some(Length::px(10.0));
  style.height_keyword = None;
  style.margin_top = Some(Length::px(0.0));
  style.margin_bottom = Some(Length::px(0.0));
  style.margin_left = Some(Length::px(0.0));
  style.margin_right = Some(Length::px(0.0));

  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

fn float_block_with_writing_mode(float: Float, writing_mode: WritingMode, direction: Direction) -> BoxNode {
  let mut style = block_style();
  style.float = float;
  style.clear = Clear::Both;
  style.writing_mode = writing_mode;
  style.direction = direction;
  style.width = Some(Length::px(50.0));
  style.width_keyword = None;
  style.height = Some(Length::px(10.0));
  style.height_keyword = None;
  style.margin_top = Some(Length::px(0.0));
  style.margin_bottom = Some(Length::px(0.0));
  style.margin_left = Some(Length::px(0.0));
  style.margin_right = Some(Length::px(0.0));

  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

fn find_by_style<'a>(node: &'a FragmentNode, predicate: &impl Fn(&ComputedStyle) -> bool) -> Option<&'a FragmentNode> {
  if let Some(style) = node.style.as_ref() {
    if predicate(style) {
      return Some(node);
    }
  }
  node.children.iter().find_map(|child| find_by_style(child, predicate))
}

#[test]
fn rtl_float_left_right_are_physical_and_inline_start_end_are_logical() {
  let mut root_style = block_style();
  root_style.direction = Direction::Rtl;
  let root_style = Arc::new(root_style);

  let root = BoxNode::new_block(
    root_style,
    FormattingContextType::Block,
    vec![
      float_block(Float::Left),
      float_block(Float::Right),
      float_block(Float::InlineStart),
      float_block(Float::InlineEnd),
    ],
  );
  let tree = BoxTree::new(root);

  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let float_left = find_by_style(&fragment, &|s| s.float == Float::Left).expect("float:left");
  let float_right = find_by_style(&fragment, &|s| s.float == Float::Right).expect("float:right");
  let float_inline_start =
    find_by_style(&fragment, &|s| s.float == Float::InlineStart).expect("float:inline-start");
  let float_inline_end =
    find_by_style(&fragment, &|s| s.float == Float::InlineEnd).expect("float:inline-end");

  let x_left = float_left.bounds.x();
  let x_right = float_right.bounds.x();
  let x_inline_start = float_inline_start.bounds.x();
  let x_inline_end = float_inline_end.bounds.x();

  assert!(
    (x_left - x_inline_end).abs() <= EPS,
    "expected float:left and float:inline-end to share the physical left edge in RTL (left={x_left:.2}, inline-end={x_inline_end:.2})"
  );
  assert!(
    (x_right - x_inline_start).abs() <= EPS,
    "expected float:right and float:inline-start to share the physical right edge in RTL (right={x_right:.2}, inline-start={x_inline_start:.2})"
  );

  let container_width = fragment.bounds.width();
  let float_width = float_left.bounds.width();
  let expected_delta = (container_width - float_width).max(0.0);

  assert!(
    x_right > x_left + expected_delta - EPS,
    "expected right-side float x to exceed left-side float x by ~{expected_delta:.2} (left={x_left:.2}, right={x_right:.2}, container={container_width:.2}, float_w={float_width:.2})",
  );
  assert!(
    ((x_right - x_left) - expected_delta).abs() <= EPS,
    "expected float:right to be offset from float:left by ~{expected_delta:.2} (got delta {:.2})",
    x_right - x_left
  );
}

#[test]
fn rtl_clear_inline_start_clears_inline_start_floats() {
  let mut root_style = block_style();
  root_style.direction = Direction::Rtl;
  let root_style = Arc::new(root_style);

  let mut float_style = block_style();
  float_style.float = Float::InlineStart;
  float_style.width = Some(Length::px(50.0));
  float_style.width_keyword = None;
  float_style.height = Some(Length::px(20.0));
  float_style.height_keyword = None;
  float_style.clear = Clear::None;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let mut block_style = block_style();
  block_style.clear = Clear::InlineStart;
  block_style.height = Some(Length::px(10.0));
  block_style.height_keyword = None;
  let clear_box = BoxNode::new_block(Arc::new(block_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![float_box, clear_box]);
  let tree = BoxTree::new(root);

  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let cleared = find_by_style(&fragment, &|s| s.clear == Clear::InlineStart)
    .expect("clear:inline-start fragment");
  assert!(
    (cleared.bounds.y() - 20.0).abs() <= EPS,
    "expected clear:inline-start block to be placed below the inline-start float in RTL (got y={:.2})",
    cleared.bounds.y()
  );
}

#[test]
fn rtl_clear_left_does_not_clear_inline_start_floats() {
  let mut root_style = block_style();
  root_style.direction = Direction::Rtl;
  let root_style = Arc::new(root_style);

  let mut float_style = block_style();
  float_style.float = Float::InlineStart;
  float_style.width = Some(Length::px(50.0));
  float_style.width_keyword = None;
  float_style.height = Some(Length::px(20.0));
  float_style.height_keyword = None;
  float_style.clear = Clear::None;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let mut block_style = block_style();
  block_style.clear = Clear::Left;
  block_style.height = Some(Length::px(10.0));
  block_style.height_keyword = None;
  let clear_box = BoxNode::new_block(Arc::new(block_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![float_box, clear_box]);
  let tree = BoxTree::new(root);

  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let cleared = find_by_style(&fragment, &|s| s.clear == Clear::Left).expect("clear:left fragment");
  assert!(
    cleared.bounds.y().abs() <= EPS,
    "expected clear:left block not to clear an inline-start float in RTL (got y={:.2})",
    cleared.bounds.y()
  );
}

#[test]
fn vertical_writing_mode_inline_start_end_float_to_top_and_bottom_in_ltr() {
  let writing_mode = WritingMode::VerticalRl;
  let direction = Direction::Ltr;

  let mut root_style = block_style();
  root_style.writing_mode = writing_mode;
  root_style.direction = direction;
  root_style.width = Some(Length::px(200.0));
  root_style.width_keyword = None;
  root_style.height = Some(Length::px(200.0));
  root_style.height_keyword = None;
  root_style.margin_top = Some(Length::px(0.0));
  root_style.margin_bottom = Some(Length::px(0.0));
  root_style.margin_left = Some(Length::px(0.0));
  root_style.margin_right = Some(Length::px(0.0));
  let root_style = Arc::new(root_style);

  let root = BoxNode::new_block(
    root_style,
    FormattingContextType::Block,
    vec![
      float_block_with_writing_mode(Float::InlineStart, writing_mode, direction),
      float_block_with_writing_mode(Float::InlineEnd, writing_mode, direction),
    ],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::definite(200.0, 200.0);

  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let float_inline_start =
    find_by_style(&fragment, &|s| s.float == Float::InlineStart).expect("float:inline-start");
  let float_inline_end =
    find_by_style(&fragment, &|s| s.float == Float::InlineEnd).expect("float:inline-end");

  let container_height = fragment.bounds.height();
  let float_height = float_inline_start.bounds.height();

  assert!(
    float_inline_start.bounds.y().abs() <= EPS,
    "expected float:inline-start to align with physical top in vertical writing mode LTR (y={:.2})",
    float_inline_start.bounds.y()
  );
  assert!(
    (float_inline_end.bounds.y() - (container_height - float_height)).abs() <= EPS,
    "expected float:inline-end to align with physical bottom in vertical writing mode LTR (y={:.2}, container_h={:.2}, float_h={:.2})",
    float_inline_end.bounds.y(),
    container_height,
    float_height
  );
}

#[test]
fn vertical_writing_mode_inline_start_end_flip_in_rtl() {
  let writing_mode = WritingMode::VerticalRl;
  let direction = Direction::Rtl;

  let mut root_style = block_style();
  root_style.writing_mode = writing_mode;
  root_style.direction = direction;
  root_style.width = Some(Length::px(200.0));
  root_style.width_keyword = None;
  root_style.height = Some(Length::px(200.0));
  root_style.height_keyword = None;
  root_style.margin_top = Some(Length::px(0.0));
  root_style.margin_bottom = Some(Length::px(0.0));
  root_style.margin_left = Some(Length::px(0.0));
  root_style.margin_right = Some(Length::px(0.0));
  let root_style = Arc::new(root_style);

  let root = BoxNode::new_block(
    root_style,
    FormattingContextType::Block,
    vec![
      float_block_with_writing_mode(Float::InlineStart, writing_mode, direction),
      float_block_with_writing_mode(Float::InlineEnd, writing_mode, direction),
    ],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::definite(200.0, 200.0);

  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let float_inline_start =
    find_by_style(&fragment, &|s| s.float == Float::InlineStart).expect("float:inline-start");
  let float_inline_end =
    find_by_style(&fragment, &|s| s.float == Float::InlineEnd).expect("float:inline-end");

  let container_height = fragment.bounds.height();
  let float_height = float_inline_start.bounds.height();

  assert!(
    (float_inline_start.bounds.y() - (container_height - float_height)).abs() <= EPS,
    "expected float:inline-start to align with physical bottom in vertical writing mode RTL (y={:.2}, container_h={:.2}, float_h={:.2})",
    float_inline_start.bounds.y(),
    container_height,
    float_height
  );
  assert!(
    float_inline_end.bounds.y().abs() <= EPS,
    "expected float:inline-end to align with physical top in vertical writing mode RTL (y={:.2})",
    float_inline_end.bounds.y()
  );
}

#[test]
fn vertical_writing_mode_clear_inline_start_clears_inline_start_floats() {
  let writing_mode = WritingMode::VerticalRl;
  let direction = Direction::Ltr;

  let mut root_style = block_style();
  root_style.writing_mode = writing_mode;
  root_style.direction = direction;
  root_style.width = Some(Length::px(200.0));
  root_style.width_keyword = None;
  root_style.height = Some(Length::px(200.0));
  root_style.height_keyword = None;
  root_style.margin_top = Some(Length::px(0.0));
  root_style.margin_bottom = Some(Length::px(0.0));
  root_style.margin_left = Some(Length::px(0.0));
  root_style.margin_right = Some(Length::px(0.0));
  let root_style = Arc::new(root_style);

  let mut float_style = block_style();
  float_style.writing_mode = writing_mode;
  float_style.direction = direction;
  float_style.float = Float::InlineStart;
  float_style.clear = Clear::None;
  float_style.width = Some(Length::px(50.0));
  float_style.width_keyword = None;
  float_style.height = Some(Length::px(20.0));
  float_style.height_keyword = None;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let mut clear_style = block_style();
  clear_style.writing_mode = writing_mode;
  clear_style.direction = direction;
  clear_style.clear = Clear::InlineStart;
  clear_style.height = Some(Length::px(10.0));
  clear_style.height_keyword = None;
  let clear_box = BoxNode::new_block(Arc::new(clear_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    root_style,
    FormattingContextType::Block,
    vec![float_box, clear_box],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::definite(200.0, 200.0);

  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let float_inline_start =
    find_by_style(&fragment, &|s| s.float == Float::InlineStart).expect("float:inline-start");
  let cleared =
    find_by_style(&fragment, &|s| s.clear == Clear::InlineStart).expect("clear:inline-start");

  // In vertical-rl, the block axis is horizontal and progresses right-to-left. Clearance therefore
  // shifts the cleared block leftward by the float's block-size (represented as the float's
  // physical width after axis conversion).
  let container_width = fragment.bounds.width();
  let float_block_size = float_inline_start.bounds.width();
  let cleared_block_size = cleared.bounds.width();
  let expected_x = container_width - float_block_size - cleared_block_size;

  assert!(
    (cleared.bounds.x() - expected_x).abs() <= EPS,
    "expected clear:inline-start block to be shifted left by clearance in vertical writing mode (got x={:.2}, expected {:.2}; container_w={:.2}, float_w={:.2}, cleared_w={:.2})",
    cleared.bounds.x(),
    expected_x,
    container_width,
    float_block_size,
    cleared_block_size
  );
}

#[test]
fn vertical_writing_mode_clear_inline_start_does_not_clear_inline_end_floats() {
  let writing_mode = WritingMode::VerticalRl;
  let direction = Direction::Ltr;

  let mut root_style = block_style();
  root_style.writing_mode = writing_mode;
  root_style.direction = direction;
  root_style.width = Some(Length::px(200.0));
  root_style.width_keyword = None;
  root_style.height = Some(Length::px(200.0));
  root_style.height_keyword = None;
  root_style.margin_top = Some(Length::px(0.0));
  root_style.margin_bottom = Some(Length::px(0.0));
  root_style.margin_left = Some(Length::px(0.0));
  root_style.margin_right = Some(Length::px(0.0));
  let root_style = Arc::new(root_style);

  let mut float_style = block_style();
  float_style.writing_mode = writing_mode;
  float_style.direction = direction;
  float_style.float = Float::InlineEnd;
  float_style.clear = Clear::None;
  float_style.width = Some(Length::px(50.0));
  float_style.width_keyword = None;
  float_style.height = Some(Length::px(20.0));
  float_style.height_keyword = None;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let mut clear_style = block_style();
  clear_style.writing_mode = writing_mode;
  clear_style.direction = direction;
  clear_style.clear = Clear::InlineStart;
  clear_style.height = Some(Length::px(10.0));
  clear_style.height_keyword = None;
  let clear_box = BoxNode::new_block(Arc::new(clear_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    root_style,
    FormattingContextType::Block,
    vec![float_box, clear_box],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::definite(200.0, 200.0);

  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let cleared =
    find_by_style(&fragment, &|s| s.clear == Clear::InlineStart).expect("clear:inline-start");

  let container_width = fragment.bounds.width();
  let cleared_block_size = cleared.bounds.width();
  let expected_x = container_width - cleared_block_size;

  assert!(
    (cleared.bounds.x() - expected_x).abs() <= EPS,
    "expected clear:inline-start not to clear an inline-end float in vertical writing mode (got x={:.2}, expected {:.2})",
    cleared.bounds.x(),
    expected_x,
  );
}
