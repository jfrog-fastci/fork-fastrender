use std::sync::Arc;

use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::factory::FormattingContextFactory;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::float::{Clear, Float};
use fastrender::style::types::{BorderStyle, LineHeight, Overflow, WritingMode};
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
fn root_bottom_margin_does_not_collapse_with_last_child() {
  let mut child_style = block_style_with_height(Some(10.0));
  child_style.margin_bottom = Some(Length::px(20.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![child],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  assert_approx(
    fragment.bounds.height(),
    30.0,
    "expected root to include last child's bottom margin (no parent/child collapse at root)",
  );
}

#[test]
fn flex_item_margins_do_not_collapse_with_children() {
  let mut inner_style = block_style_with_height(Some(10.0));
  inner_style.margin_top = Some(Length::px(20.0));
  inner_style.margin_bottom = Some(Length::px(20.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let outer = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![inner],
  );

  // Assign stable ids so the flex item is not treated as the document root (root id == 1 has its
  // own margin-collapsing rules).
  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![outer],
  );
  let tree = BoxTree::new(root);
  let flex_item = &tree.root.children[0];

  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::for_flex_item_with_factory(FormattingContextFactory::new())
    .layout(flex_item, &constraints)
    .expect("layout");

  let inner_fragment = &fragment.children[0];
  assert_approx(
    inner_fragment.bounds.y(),
    20.0,
    "expected a flex item to establish an independent formatting context (no parent/child margin collapse at top)",
  );
  assert_approx(
    fragment.bounds.height(),
    50.0,
    "expected a flex item to include its last child's bottom margin (no parent/child margin collapse at bottom)",
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
fn border_prevents_parent_first_child_margin_collapse() {
  let prev = block_with_height_and_margins(10.0, 0.0, 0.0);

  let mut inner_style = block_style_with_height(Some(10.0));
  inner_style.margin_top = Some(Length::px(20.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut outer_style = block_style_with_height(None);
  outer_style.border_top_style = BorderStyle::Solid;
  outer_style.border_top_width = Length::px(10.0);
  let outer = BoxNode::new_block(Arc::new(outer_style), FormattingContextType::Block, vec![inner]);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![prev, outer],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let prev_fragment = &fragment.children[0];
  let outer_fragment = &fragment.children[1];
  let inner_fragment = &outer_fragment.children[0];

  assert_approx(
    outer_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    0.0,
    "expected border-top to prevent parent/first-child margin collapse affecting sibling placement",
  );
  assert_approx(
    inner_fragment.bounds.y(),
    30.0,
    "expected border-top to keep the child's margin inside the parent (border + margin)",
  );
}

#[test]
fn padding_prevents_parent_first_child_margin_collapse() {
  let prev = block_with_height_and_margins(10.0, 0.0, 0.0);

  let mut inner_style = block_style_with_height(Some(10.0));
  inner_style.margin_top = Some(Length::px(20.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut outer_style = block_style_with_height(None);
  outer_style.padding_top = Length::px(10.0);
  let outer = BoxNode::new_block(Arc::new(outer_style), FormattingContextType::Block, vec![inner]);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![prev, outer],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let prev_fragment = &fragment.children[0];
  let outer_fragment = &fragment.children[1];
  let inner_fragment = &outer_fragment.children[0];

  assert_approx(
    outer_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    0.0,
    "expected padding-top to prevent parent/first-child margin collapse affecting sibling placement",
  );
  assert_approx(
    inner_fragment.bounds.y(),
    30.0,
    "expected padding-top to keep the child's margin inside the parent (padding + margin)",
  );
}

#[test]
fn overflow_creates_bfc_and_prevents_parent_child_margin_collapse() {
  let prev = block_with_height_and_margins(10.0, 0.0, 0.0);

  let mut inner_style = block_style_with_height(Some(10.0));
  inner_style.margin_top = Some(Length::px(20.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut outer_style = block_style_with_height(None);
  outer_style.overflow_y = Overflow::Hidden;
  let outer = BoxNode::new_block(Arc::new(outer_style), FormattingContextType::Block, vec![inner]);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![prev, outer],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let prev_fragment = &fragment.children[0];
  let outer_fragment = &fragment.children[1];
  let inner_fragment = &outer_fragment.children[0];

  assert_approx(
    outer_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    0.0,
    "expected BFC root to prevent parent/first-child margin collapse affecting sibling placement",
  );
  assert_approx(
    inner_fragment.bounds.y(),
    20.0,
    "expected BFC root to keep the child's margin inside the parent",
  );
}

#[test]
fn flow_root_creates_bfc_and_prevents_parent_child_margin_collapse() {
  let prev = block_with_height_and_margins(10.0, 0.0, 0.0);

  let mut inner_style = block_style_with_height(Some(10.0));
  inner_style.margin_top = Some(Length::px(20.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut outer_style = block_style_with_height(None);
  outer_style.display = Display::FlowRoot;
  let outer = BoxNode::new_block(Arc::new(outer_style), FormattingContextType::Block, vec![inner]);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![prev, outer],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let prev_fragment = &fragment.children[0];
  let outer_fragment = &fragment.children[1];
  let inner_fragment = &outer_fragment.children[0];

  assert_approx(
    outer_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    0.0,
    "expected flow-root to prevent parent/first-child margin collapse affecting sibling placement",
  );
  assert_approx(
    inner_fragment.bounds.y(),
    20.0,
    "expected flow-root to keep the child's margin inside the parent",
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
fn border_prevents_parent_last_child_margin_collapse() {
  let mut child_style = block_style_with_height(Some(10.0));
  child_style.margin_bottom = Some(Length::px(20.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let mut outer_style = block_style_with_height(None);
  outer_style.border_bottom_style = BorderStyle::Solid;
  outer_style.border_bottom_width = Length::px(10.0);
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
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
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let outer_fragment = &fragment.children[0];
  let next_fragment = &fragment.children[1];

  assert_approx(
    outer_fragment.bounds.height(),
    40.0,
    "expected border-bottom to keep last child's margin inside the parent (height=child+margin+border)",
  );
  assert_approx(
    next_fragment.bounds.y() - outer_fragment.bounds.max_y(),
    0.0,
    "expected border-bottom to prevent parent/last-child margin collapse affecting sibling placement",
  );
}

#[test]
fn padding_prevents_parent_last_child_margin_collapse() {
  let mut child_style = block_style_with_height(Some(10.0));
  child_style.margin_bottom = Some(Length::px(20.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let mut outer_style = block_style_with_height(None);
  outer_style.padding_bottom = Length::px(10.0);
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
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
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let outer_fragment = &fragment.children[0];
  let next_fragment = &fragment.children[1];

  assert_approx(
    outer_fragment.bounds.height(),
    40.0,
    "expected padding-bottom to keep last child's margin inside the parent (height=child+margin+padding)",
  );
  assert_approx(
    next_fragment.bounds.y() - outer_fragment.bounds.max_y(),
    0.0,
    "expected padding-bottom to prevent parent/last-child margin collapse affecting sibling placement",
  );
}

#[test]
fn bfc_prevents_parent_last_child_margin_collapse() {
  let mut child_style = block_style_with_height(Some(10.0));
  child_style.margin_bottom = Some(Length::px(20.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let mut outer_style = block_style_with_height(None);
  outer_style.overflow_y = Overflow::Hidden;
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
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
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let outer_fragment = &fragment.children[0];
  let next_fragment = &fragment.children[1];

  assert_approx(
    outer_fragment.bounds.height(),
    30.0,
    "expected BFC root to keep last child's margin inside the parent (height=child+margin)",
  );
  assert_approx(
    next_fragment.bounds.y() - outer_fragment.bounds.max_y(),
    0.0,
    "expected BFC root to prevent parent/last-child margin collapse affecting sibling placement",
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
fn explicit_height_prevents_collapse_through_empty_blocks() {
  let red = block_with_height_and_margins(10.0, 0.0, 10.0);
  let middle = block_with_height_and_margins(10.0, 20.0, 30.0);
  let blue = block_with_height_and_margins(10.0, 40.0, 0.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![red, middle, blue],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let red_fragment = &fragment.children[0];
  let blue_fragment = &fragment.children[2];

  assert_approx(
    blue_fragment.bounds.y() - red_fragment.bounds.max_y(),
    70.0,
    "expected middle block with height to prevent through-collapse: 20 + 10 + 40 = 70",
  );
}

#[test]
fn min_height_prevents_collapse_through_empty_blocks() {
  let red = block_with_height_and_margins(10.0, 0.0, 10.0);

  let mut middle_style = block_style_with_height(None);
  middle_style.min_height = Some(Length::px(10.0));
  middle_style.min_height_keyword = None;
  middle_style.margin_top = Some(Length::px(20.0));
  middle_style.margin_bottom = Some(Length::px(30.0));
  let middle = BoxNode::new_block(Arc::new(middle_style), FormattingContextType::Block, vec![]);

  let blue = block_with_height_and_margins(10.0, 40.0, 0.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![red, middle, blue],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let red_fragment = &fragment.children[0];
  let blue_fragment = &fragment.children[2];

  assert_approx(
    blue_fragment.bounds.y() - red_fragment.bounds.max_y(),
    70.0,
    "expected middle block with min-height to prevent through-collapse: 20 + 10 + 40 = 70",
  );
}

#[test]
fn inline_content_prevents_collapse_through_empty_blocks() {
  let red = block_with_height_and_margins(10.0, 0.0, 10.0);

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Inline;
  // Give the inline formatting context a deterministic 0-height line box so this test doesn't
  // depend on font metrics.
  inline_style.font_size = 0.0;
  inline_style.line_height = LineHeight::Number(0.0);
  let inline_text = BoxNode::new_text(Arc::new(inline_style), "x".to_string());

  let mut middle_style = block_style_with_height(Some(0.0));
  middle_style.margin_top = Some(Length::px(20.0));
  middle_style.margin_bottom = Some(Length::px(30.0));
  let middle = BoxNode::new_block(
    Arc::new(middle_style),
    FormattingContextType::Block,
    vec![inline_text],
  );

  let blue = block_with_height_and_margins(10.0, 40.0, 0.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![red, middle, blue],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let red_fragment = &fragment.children[0];
  let blue_fragment = &fragment.children[2];

  assert_approx(
    blue_fragment.bounds.y() - red_fragment.bounds.max_y(),
    60.0,
    "expected inline content to prevent through-collapse: max(10,20)=20 + 0 + max(30,40)=40",
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
fn mixed_signs_collapse_across_three_or_more_adjoining_margins() {
  // Collapsing across an empty block joins all adjoining margins into one chain:
  //   30 (a mb) + max(-10, -20) (negatives) => 30 + (-20) = 10
  let a = block_with_height_and_margins(10.0, 0.0, 30.0);
  let empty = empty_block_with_margins(-10.0, 5.0);
  let b = block_with_height_and_margins(10.0, -20.0, 0.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![a, empty, b],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let a_fragment = &fragment.children[0];
  let b_fragment = &fragment.children[2];
  assert_approx(
    b_fragment.bounds.y() - a_fragment.bounds.max_y(),
    10.0,
    "expected mixed-sign collapse across multiple adjoining margins to use largest positive + most negative",
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

#[test]
fn vertical_writing_mode_uses_block_start_end_edges_for_margin_collapse() {
  // In vertical-rl writing modes, the block axis is horizontal and block-start is on the right.
  // Padding on the block-start edge must prevent parent/first-child margin collapse.
  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.writing_mode = WritingMode::VerticalRl;
  inner_style.width = Some(Length::px(10.0));
  inner_style.height = Some(Length::px(10.0));
  inner_style.width_keyword = None;
  inner_style.height_keyword = None;
  inner_style.margin_right = Some(Length::px(20.0)); // block-start margin in vertical-rl
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Block;
  outer_style.writing_mode = WritingMode::VerticalRl;
  outer_style.width = Some(Length::px(100.0));
  outer_style.height = Some(Length::px(50.0));
  outer_style.width_keyword = None;
  outer_style.height_keyword = None;
  outer_style.padding_right = Length::px(10.0); // block-start padding in vertical-rl
  let outer = BoxNode::new_block(Arc::new(outer_style), FormattingContextType::Block, vec![inner]);

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.writing_mode = WritingMode::VerticalRl;
  root_style.width = Some(Length::px(100.0));
  root_style.height = Some(Length::px(200.0));
  root_style.width_keyword = None;
  root_style.height_keyword = None;
  let root = BoxNode::new_block(Arc::new(root_style), FormattingContextType::Block, vec![outer]);

  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(100.0),
    AvailableSpace::Definite(200.0),
  );
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let outer_fragment = &fragment.children[0];
  let inner_fragment = &outer_fragment.children[0];

  // `width` is the content-box size. In vertical-rl, padding-right contributes to the block axis
  // (horizontal) border-box size, so the child position is anchored from the parent’s right edge:
  //   x = parent_border_width - padding_right - margin_right - child_border_width
  let expected_x = outer_fragment.bounds.width() - 10.0 - 20.0 - inner_fragment.bounds.width();
  assert_approx(
    inner_fragment.bounds.x(),
    expected_x,
    "expected vertical writing mode to treat right padding/margin as block-start separation for margin collapse",
  );
}

#[test]
fn flex_item_establishes_bfc_for_margin_collapse() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;

  let mut inner_style = block_style_with_height(Some(10.0));
  inner_style.margin_top = Some(Length::px(20.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let item = BoxNode::new_block(
    Arc::new(item_style),
    FormattingContextType::Block,
    vec![inner],
  );
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![item],
  );

  let fragment = FlexFormattingContext::new()
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite),
    )
    .expect("layout");

  let item_fragment = &fragment.children[0];
  let inner_fragment = &item_fragment.children[0];
  assert_approx(
    inner_fragment.bounds.y(),
    20.0,
    "expected flex items to establish a BFC, preventing parent/child margin collapsing",
  );
}
