use std::sync::Arc;

use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::factory::FormattingContextFactory;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::float::{Clear, Float};
use fastrender::style::types::{AspectRatio, BorderStyle, LineHeight, Overflow, WritingMode};
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

fn whitespace_only_block_with_margins(margin_top: f32, margin_bottom: f32) -> BoxNode {
  let mut block_style = block_style_with_height(None);
  block_style.margin_top = Some(Length::px(margin_top));
  block_style.margin_bottom = Some(Length::px(margin_bottom));

  let mut inline_style = ComputedStyle::default();
  inline_style.display = Display::Inline;
  let inline_style = Arc::new(inline_style);

  let text = BoxNode::new_text(inline_style.clone(), " \n\t".to_string());
  let anon_inline = BoxNode::new_anonymous_inline(inline_style, vec![text]);
  BoxNode::new_block(
    Arc::new(block_style),
    FormattingContextType::Block,
    vec![anon_inline],
  )
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
fn parent_first_child_collapse_through_empty_block_does_not_offset_floats() {
  let prev = block_with_height_and_margins(0.0, 0.0, 0.0);

  // An empty first child should collapse its top/bottom margins through itself and out of the
  // parent. Floats that follow must start at the parent's block start, not below the collapsed
  // margin chain (CSS 2.1 §8.3.1).
  let empty = empty_block_with_margins(20.0, 30.0);

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::Block;
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(10.0));
  float_style.height = Some(Length::px(10.0));
  float_style.width_keyword = None;
  float_style.height_keyword = None;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  // Ensure the parent is not collapsible-through itself: a block that contains only floats has a
  // used height of 0 in CSS2.1, but the presence of `clear` after floats (like a footer) prevents
  // collapsing-through and keeps the parent's collapsed margins meaningful for sibling placement.
  let mut clear_style = block_style_with_height(Some(0.0));
  clear_style.clear = Clear::Both;
  let clear_block = BoxNode::new_block(Arc::new(clear_style), FormattingContextType::Block, vec![]);

  let outer = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![empty, float_box, clear_block],
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

  assert_approx(
    outer_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    30.0,
    "expected the empty child's margins to collapse into the parent's own top margin (max(20,30)=30)",
  );
  assert!(
    outer_fragment.children.len() >= 2,
    "expected outer to have both the empty child and float fragments"
  );
  let float_fragment = &outer_fragment.children[1];
  assert_approx(
    float_fragment.bounds.y(),
    0.0,
    "expected float to start at y=0 inside the parent after parent/first-child margin collapsing",
  );
}

#[test]
fn clearance_prevents_parent_first_child_margin_collapse_past_floats() {
  let prev = block_with_height_and_margins(0.0, 0.0, 0.0);

  // The parent should collapse its own margin-top with the empty child's margins, but must not
  // further collapse with a later in-flow block whose `clear` introduces clearance against floats.
  // Otherwise, the cleared block's margin-top would incorrectly be hoisted to the parent's top
  // edge (CSS 2.1 §9.5.2 / §8.3.1).
  let empty = empty_block_with_margins(10.0, 10.0);

  let mut float_style = block_style_with_height(Some(50.0));
  float_style.width = Some(Length::px(10.0));
  float_style.width_keyword = None;
  float_style.float = Float::Left;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let mut cleared_style = block_style_with_height(Some(0.0));
  cleared_style.margin_top = Some(Length::px(20.0));
  cleared_style.clear = Clear::Left;
  let cleared = BoxNode::new_block(Arc::new(cleared_style), FormattingContextType::Block, vec![]);

  let outer = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![empty, float_box, cleared],
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

  assert_approx(
    outer_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    10.0,
    "expected parent/first-child collapsing to ignore margins after clearance (max(0,10)=10)",
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
fn bfc_with_only_floats_is_not_treated_as_empty_for_margin_collapse() {
  // Floats do not generate line boxes, but when a container establishes a BFC (e.g. via
  // `overflow:hidden`) they contribute to the container's auto height. Such boxes should therefore
  // stop "collapsing through empty blocks" logic, otherwise ancestor margins can collapse past a
  // float-containing BFC and incorrectly offset the document (regression seen on sqlite.org where
  // <body> was shifted down by the first <p>'s 1em margin-top).

  let mut float_style = block_style_with_height(Some(10.0));
  float_style.width = Some(Length::px(10.0));
  float_style.width_keyword = None;
  float_style.float = Float::Left;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let mut bfc_style = block_style_with_height(None);
  bfc_style.overflow_y = Overflow::Hidden;
  let bfc = BoxNode::new_block(Arc::new(bfc_style), FormattingContextType::Block, vec![float_box]);

  // Wrapper that would be considered collapsible-through if the BFC child were misclassified as
  // empty.
  let wrapper = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![bfc],
  );

  let mut para_style = block_style_with_height(Some(10.0));
  para_style.margin_top = Some(Length::px(16.0));
  let para = BoxNode::new_block(Arc::new(para_style), FormattingContextType::Block, vec![]);

  // Root (id=1 in BoxTree) should not collapse with body, so any collapsed margin chain inside the
  // body affects its y offset.
  let body = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![wrapper, para],
  );
  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![body],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let body_fragment = &fragment.children[0];
  assert_approx(
    body_fragment.bounds.y(),
    0.0,
    "expected float-containing BFC to prevent collapsing-through from picking up the following paragraph's margin-top",
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
fn negative_trailing_margin_can_shrink_auto_height_when_not_collapsing_with_parent() {
  // Regression: a negative bottom margin on the last in-flow child should be able to *shrink* the
  // parent's used auto height when the child's margin does not collapse with the parent (e.g. the
  // parent establishes a new BFC via overflow != visible).
  //
  // Chrome relies on this behavior for layout of `forbes.com` (the edition selector bar uses
  // negative vertical margins to pull the next section upward).
  let mut inner_style = block_style_with_height(Some(41.0));
  inner_style.margin_top = Some(Length::px(-8.0));
  inner_style.margin_bottom = Some(Length::px(-8.0));
  let inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);

  let mut outer_style = block_style_with_height(None);
  outer_style.overflow_x = Overflow::Hidden;
  outer_style.overflow_y = Overflow::Hidden;
  let outer = BoxNode::new_block(Arc::new(outer_style), FormattingContextType::Block, vec![inner]);

  let after = block_with_height_and_margins(10.0, 0.0, 0.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![outer, after],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let outer_fragment = &fragment.children[0];
  let inner_fragment = &outer_fragment.children[0];
  let after_fragment = &fragment.children[1];

  assert_approx(
    outer_fragment.bounds.height(),
    25.0,
    "expected negative trailing margin to shrink the parent's used height (-8 + 41 + -8 = 25)",
  );
  assert_approx(
    inner_fragment.bounds.max_y(),
    33.0,
    "expected the child's border box to protrude below the parent's used height",
  );
  assert_approx(
    after_fragment.bounds.y() - outer_fragment.bounds.max_y(),
    0.0,
    "expected following siblings to be positioned using the parent's used height (not the protruding child border box)",
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
fn collapse_through_whitespace_only_blocks() {
  let red = block_with_height_and_margins(10.0, 0.0, 10.0);
  let empty = whitespace_only_block_with_margins(20.0, 30.0);
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
    "expected whitespace-only empty blocks to collapse through (max(10,30,40)=40)",
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
fn sibling_positive_margins_collapse_to_max() {
  let header = block_with_height_and_margins(50.0, 0.0, 0.0);
  let a = block_with_height_and_margins(27.0, 16.0, 23.0);

  let mut b_style = block_style_with_height(Some(4.0));
  b_style.margin_top = Some(Length::px(21.0));
  b_style.width = Some(Length::px(40.0));
  b_style.width_keyword = None;
  b_style.margin_left = None;
  b_style.margin_right = None;
  let b = BoxNode::new_block(Arc::new(b_style), FormattingContextType::Block, vec![]);

  let mut inner_style = block_style_with_height(None);
  inner_style.padding_top = Length::px(50.0);
  inner_style.padding_bottom = Length::px(60.0);
  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Block,
    vec![header, a, b],
  );

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![inner],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let inner_fragment = &fragment.children[0];
  let a_fragment = &inner_fragment.children[1];
  let b_fragment = &inner_fragment.children[2];
  assert_approx(
    b_fragment.bounds.y() - a_fragment.bounds.max_y(),
    23.0,
    "expected sibling margins to collapse: max(23, 21) = 23",
  );
}

#[test]
fn negative_trailing_margins_can_shrink_parent_height_when_bottom_separated() {
  // Layout engines must allow negative margins on an *empty last in-flow block* (e.g. a
  // `::after { content:''; display:block; margin-top:-... }` cap-height trim) to pull the
  // in-flow cursor upward, reducing the parent's used height and shifting following siblings.
  //
  // This is spec-correct with default `overflow: visible`: earlier in-flow content may overflow
  // the parent's border box.
  let inner = block_with_height_and_margins(10.0, 0.0, 0.0);
  let trim = empty_block_with_margins(-5.0, 0.0);

  let mut outer_style = block_style_with_height(None);
  outer_style.padding_bottom = Length::px(1.0);
  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Block,
    vec![inner, trim],
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
    6.0,
    "expected negative trailing margin to shrink the parent's border-box height (10 + -5 + padding-bottom 1)",
  );
  assert_approx(
    next_fragment.bounds.y(),
    outer_fragment.bounds.max_y(),
    "expected next sibling to follow the shrunken parent border box",
  );
}

#[test]
fn sibling_margin_collapse_is_not_broken_by_external_float_base_rounding() {
  let spacer = block_with_height_and_margins(16_777_216.0, 0.0, 0.0);
  let a = block_with_height_and_margins(10.1, 0.0, 23.0);
  let b = block_with_height_and_margins(4.0, 21.0, 0.0);

  let inner = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![a, b],
  );

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![spacer, inner],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let inner_fragment = &fragment.children[1];
  let a_fragment = &inner_fragment.children[0];
  let b_fragment = &inner_fragment.children[1];
  assert_approx(
    b_fragment.bounds.y() - a_fragment.bounds.max_y(),
    23.0,
    "expected sibling margins to collapse (external float base should not introduce clearance)",
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
fn aspect_ratio_prevents_collapse_through_empty_blocks() {
  // A common modern pattern uses `aspect-ratio` on an otherwise-empty element to reserve space for
  // absolutely positioned content (e.g. images). Even though `height:auto` and there is no in-flow
  // content, the used height is non-zero, so margins must not collapse *through* the box.
  //
  // If we incorrectly treat this box as "collapsible-through", the block formatting context can
  // remain in the "at start" state and the next in-flow child may be positioned as if it were the
  // first child, dropping sibling margin collapsing.
  let mut a_style = block_style_with_height(None);
  a_style.aspect_ratio = AspectRatio::Ratio(1.0);
  a_style.margin_bottom = Some(Length::px(30.0));
  let a = BoxNode::new_block(Arc::new(a_style), FormattingContextType::Block, vec![]);

  let b = block_with_height_and_margins(10.0, -10.0, 0.0);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![a, b],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let a_fragment = &fragment.children[0];
  let b_fragment = &fragment.children[1];

  assert_approx(
    a_fragment.bounds.height(),
    100.0,
    "expected aspect-ratio to produce a non-zero used height",
  );
  assert_approx(
    b_fragment.bounds.y() - a_fragment.bounds.max_y(),
    20.0,
    "expected sibling margins to collapse (30 + -10 = 20) instead of collapsing through the aspect-ratio box",
  );
}

#[test]
fn overflow_bfc_with_floats_prevents_collapse_through() {
  // Regression: block formatting context roots that "self-clear" floats (e.g. `overflow:hidden`)
  // have a non-zero used height and must not be treated as collapsible-through for margin
  // collapsing (CSS 2.1 §8.3.1).
  //
  // This matches patterns like sqlite.org's header navigation: if we incorrectly treat the float
  // container as collapsible-through, the first real block's top margin can collapse all the way
  // out of the body, shifting the entire page down by a line-height.
  let mut float_style = block_style_with_height(Some(10.0));
  float_style.width = Some(Length::px(10.0));
  float_style.width_keyword = None;
  float_style.float = Float::Left;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let mut bfc_style = block_style_with_height(None);
  bfc_style.overflow_x = Overflow::Hidden;
  bfc_style.overflow_y = Overflow::Hidden;
  let bfc_container = BoxNode::new_block(
    Arc::new(bfc_style),
    FormattingContextType::Block,
    vec![float_box],
  );

  let mut following_style = block_style_with_height(Some(10.0));
  following_style.margin_top = Some(Length::px(20.0));
  let following =
    BoxNode::new_block(Arc::new(following_style), FormattingContextType::Block, vec![]);

  let mut body_style = ComputedStyle::default();
  body_style.display = Display::Block;
  let mut body = BoxNode::new_block(
    Arc::new(body_style),
    FormattingContextType::Block,
    vec![bfc_container, following],
  );
  body.id = 2;

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  let mut root =
    BoxNode::new_block(Arc::new(root_style), FormattingContextType::Block, vec![body]);
  // Root element margins never collapse with children; mimic the real HTML root.
  root.id = 1;

  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let body_fragment = &fragment.children[0];
  assert_approx(
    body_fragment.bounds.y(),
    0.0,
    "expected float-containing BFC roots to prevent margin collapsing through earlier siblings",
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
fn clear_none_does_not_break_sibling_margin_collapse_in_nested_block() {
  // When a block formatting context reuses an ancestor float context (i.e., it does not establish a
  // new BFC), `float_base_y` can be non-zero. Clearance computations must not introduce tiny
  // positive deltas when `clear: none`, otherwise sibling margins may fail to collapse.

  // Spacer to push the nested block down, ensuring a non-zero `float_base_y`.
  let spacer = block_with_height_and_margins(50.0, 0.0, 0.0);

  // Two sibling blocks whose margins should collapse to max(6.496, 4.28736) = 6.496.
  let a = block_with_height_and_margins(56.65985, 0.0, 6.496);
  let b = block_with_height_and_margins(10.0, 4.28736, 0.0);

  let nested = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![a, b],
  );

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![spacer, nested],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let spacer_fragment = &fragment.children[0];
  let nested_fragment = &fragment.children[1];
  assert_approx(
    nested_fragment.bounds.y(),
    spacer_fragment.bounds.max_y(),
    "expected nested block to be placed after the spacer",
  );

  let a_fragment = &nested_fragment.children[0];
  let b_fragment = &nested_fragment.children[1];
  assert_approx(
    b_fragment.bounds.y() - a_fragment.bounds.max_y(),
    6.496,
    "expected sibling margins to collapse to max(6.496, 4.28736) = 6.496",
  );
}

#[test]
fn clear_with_descendant_float_prevents_collapse_through() {
  let prev = block_with_height_and_margins(100.0, 0.0, 0.0);

  let mut float_style = block_style_with_height(Some(50.0));
  float_style.width = Some(Length::px(10.0));
  float_style.width_keyword = None;
  float_style.float = Float::Left;
  let float_box = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  let float_wrapper = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![float_box],
  );
  let float_container = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![float_wrapper],
  );

  let mut clear_style = block_style_with_height(None);
  clear_style.clear = Clear::Both;
  let clear_box = BoxNode::new_block(Arc::new(clear_style), FormattingContextType::Block, vec![]);

  let mut container_style = block_style_with_height(None);
  container_style.margin_top = Some(Length::px(-20.0));
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![float_container, clear_box],
  );

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![prev, container],
  );
  let tree = BoxTree::new(root);
  let constraints = LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let prev_fragment = &fragment.children[0];
  let container_fragment = &fragment.children[1];
  assert_approx(
    container_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    -20.0,
    "expected margin-top to apply instead of collapsing through a container that gains height from clearing descendant floats",
  );
}

#[test]
fn empty_table_is_not_collapsible_through() {
  let prev = block_with_height_and_margins(100.0, 0.0, 0.0);

  let mut table_style = block_style_with_height(None);
  table_style.display = Display::Table;
  table_style.width = Some(Length::px(10.0));
  table_style.width_keyword = None;
  table_style.margin_top = Some(Length::px(-20.0));
  let table = BoxNode::new_block(Arc::new(table_style), FormattingContextType::Table, vec![]);

  let root = BoxNode::new_block(
    Arc::new(block_style_with_height(None)),
    FormattingContextType::Block,
    vec![prev, table],
  );
  let tree = BoxTree::new(root);
  let constraints =
    LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite);
  let fragment = BlockFormattingContext::new()
    .layout(&tree.root, &constraints)
    .expect("layout");

  let prev_fragment = &fragment.children[0];
  let table_fragment = &fragment.children[1];
  assert_approx(
    table_fragment.bounds.y() - prev_fragment.bounds.max_y(),
    -20.0,
    "expected tables to not be treated as collapsible-through for margin collapsing",
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
