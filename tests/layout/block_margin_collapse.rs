use std::sync::Arc;

use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::float::{Clear, Float};
use fastrender::style::values::Length;
use fastrender::{BoxNode, BoxTree, ComputedStyle, FormattingContext};

const EPS: f32 = 0.01;

fn block_style_with_height(height: Option<f32>) -> ComputedStyle {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.height = height.map(Length::px);
  style.height_keyword = None;
  style
}

fn block_with_height_and_margins(height: f32, margin_top: f32, margin_bottom: f32) -> BoxNode {
  let mut style = block_style_with_height(Some(height));
  style.margin_top = Some(Length::px(margin_top));
  style.margin_bottom = Some(Length::px(margin_bottom));
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

fn empty_block_with_margins(margin_top: f32, margin_bottom: f32) -> BoxNode {
  let mut style = block_style_with_height(None);
  style.margin_top = Some(Length::px(margin_top));
  style.margin_bottom = Some(Length::px(margin_bottom));
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

fn assert_approx(a: f32, b: f32, msg: &str) {
  assert!(
    (a - b).abs() <= EPS,
    "{} (got {:.2}, expected {:.2})",
    msg,
    a,
    b
  );
}

#[test]
fn root_margins_do_not_collapse_with_children() {
  let mut root_style = block_style_with_height(None);
  root_style.margin_top = Some(Length::px(0.0));
  let mut child_style = block_style_with_height(Some(10.0));
  child_style.margin_top = Some(Length::px(20.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![child],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");
  let child_fragment = &fragment.children[0];
  assert_approx(
    child_fragment.bounds.y(),
    20.0,
    "expected root to apply the child's top margin (no parent/child collapse)",
  );
}

#[test]
fn parent_first_child_margin_collapses() {
  let prev = block_with_height_and_margins(0.0, 0.0, 0.0);

  let mut inner_style = block_style_with_height(Some(10.0));
  inner_style.margin_top = Some(Length::px(20.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let outer = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![inner],
  );

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![prev, outer],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let prev_fragment = &fragment.children[0];
  let outer_fragment = &fragment.children[1];
  let inner_fragment = &outer_fragment.children[0];

  assert_approx(
    inner_fragment.bounds.y(),
    0.0,
    "expected first child's top margin to collapse out of the parent",
  );
  assert_approx(
    outer_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    20.0,
    "expected the collapsed margin to affect the parent's position among siblings",
  );
}

#[test]
fn parent_last_child_margin_collapses() {
  let mut child_style = block_style_with_height(Some(10.0));
  child_style.margin_bottom = Some(Length::px(20.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  let outer = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![child],
  );

  let next = block_with_height_and_margins(10.0, 0.0, 0.0);
  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![outer, next],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let outer_fragment = &fragment.children[0];
  let next_fragment = &fragment.children[1];

  assert_approx(
    outer_fragment.bounds.height(),
    10.0,
    "expected collapsed bottom margin to not contribute to parent height",
  );
  assert_approx(
    next_fragment.bounds.y() - outer_fragment.bounds.max_y(),
    20.0,
    "expected spacing to following sibling to use the collapsed bottom margin",
  );
}

#[test]
fn collapse_through_empty_blocks() {
  let red = block_with_height_and_margins(10.0, 0.0, 10.0);
  let empty = empty_block_with_margins(20.0, 30.0);
  let blue = block_with_height_and_margins(10.0, 40.0, 0.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![red, empty, blue],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let red_fragment = &fragment.children[0];
  let blue_fragment = &fragment.children[2];

  assert_approx(
    blue_fragment.bounds.y() - red_fragment.bounds.max_y(),
    40.0,
    "expected collapsed margin across empty block to be max(10,30,40)=40",
  );
}

#[test]
fn negative_margins_collapse_correctly() {
  let a = block_with_height_and_margins(10.0, 0.0, 30.0);
  let b = block_with_height_and_margins(10.0, -10.0, 0.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![a, b],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let a_fragment = &fragment.children[0];
  let b_fragment = &fragment.children[1];
  assert_approx(
    b_fragment.bounds.y() - a_fragment.bounds.max_y(),
    20.0,
    "expected margin collapse with mixed signs to add largest positive + most negative",
  );
}

#[test]
fn clearance_breaks_margin_collapse() {
  // A left float tall enough to overlap the next in-flow block.
  let mut float_style = block_style_with_height(Some(50.0));
  float_style.width = Some(Length::px(10.0));
  float_style.width_keyword = None;
  float_style.float = Float::Left;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let prev = block_with_height_and_margins(10.0, 0.0, 20.0);

  let mut cleared_style = block_style_with_height(Some(10.0));
  cleared_style.margin_top = Some(Length::px(10.0));
  cleared_style.clear = Clear::Left;
  let cleared = BoxNode::new_block(Arc::new(cleared_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![float_box, prev, cleared],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let prev_fragment = &fragment.children[1];
  let cleared_fragment = &fragment.children[2];

  // Spacing should include prev margin-bottom (20) + clearance (20) + cleared margin-top (10) = 50.
  assert_approx(
    cleared_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    50.0,
    "expected clearance to break margin collapsing with the previous sibling",
  );
}

#[test]
fn float_does_not_break_sibling_margin_collapse() {
  let a = block_with_height_and_margins(10.0, 0.0, 10.0);

  let mut float_style = block_style_with_height(Some(10.0));
  float_style.width = Some(Length::px(10.0));
  float_style.width_keyword = None;
  float_style.float = Float::Left;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let b = block_with_height_and_margins(10.0, 20.0, 0.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![a, float_box, b],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let a_fragment = &fragment.children[0];
  let b_fragment = &fragment.children[2];

  assert_approx(
    b_fragment.bounds.y() - a_fragment.bounds.max_y(),
    20.0,
    "expected float to not interrupt sibling margin collapsing (max(10,20)=20)",
  );
}

#[test]
fn trailing_margins_do_not_extend_past_floats() {
  let mut float_style = block_style_with_height(Some(100.0));
  float_style.width = Some(Length::px(10.0));
  float_style.width_keyword = None;
  float_style.float = Float::Left;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let block = block_with_height_and_margins(10.0, 0.0, 20.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![float_box, block],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  assert_approx(
    fragment.bounds.height(),
    100.0,
    "expected trailing margin to not extend past the float's bottom edge",
  );
}

#[test]
fn float_establishes_bfc_for_margin_collapse() {
  let mut inner_style = block_style_with_height(Some(10.0));
  inner_style.margin_top = Some(Length::px(20.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut float_style = block_style_with_height(None);
  float_style.width = Some(Length::px(100.0));
  float_style.width_keyword = None;
  float_style.float = Float::Left;
  let float_box = BoxNode::new_block(
    Arc::new(float_style),
    FormattingContextType::Block,
    vec![inner],
  );

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![float_box],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let float_fragment = &fragment.children[0];
  let inner_fragment = &float_fragment.children[0];
  assert_approx(
    inner_fragment.bounds.y(),
    20.0,
    "expected float to establish a BFC, preventing parent/child margin collapsing",
  );
}
