use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::float::Float;
use fastrender::style::types::Overflow;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContext;
use fastrender::FormattingContextType;
use std::sync::Arc;

#[test]
fn bfc_root_block_is_pushed_down_when_too_wide_to_fit_next_to_floats() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(50.0));
  float_style.height = Some(Length::px(20.0));
  let float_node =
    BoxNode::new_inline_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  // `overflow:hidden` establishes a BFC. Its border box must not overlap floats, so when the block
  // is too wide to fit next to a float, it must be pushed down below the float instead of being
  // shifted horizontally (which would overflow the containing block).
  let mut bfc_style = ComputedStyle::default();
  bfc_style.display = Display::Block;
  bfc_style.overflow_x = Overflow::Hidden;
  bfc_style.overflow_y = Overflow::Hidden;
  bfc_style.width = Some(Length::percent(100.0));
  bfc_style.height = Some(Length::px(10.0));
  let bfc_node = BoxNode::new_block(Arc::new(bfc_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![float_node, bfc_node],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  let bfc_frags: Vec<_> = fragment
    .children
    .iter()
    .filter(|child| (child.bounds.width() - 200.0).abs() < 0.01 && (child.bounds.height() - 10.0).abs() < 0.01)
    .collect();
  assert_eq!(
    bfc_frags.len(),
    1,
    "expected a single BFC root fragment; got {} children",
    fragment.children.len()
  );
  let bfc_frag = bfc_frags[0];
  assert!(
    bfc_frag.bounds.x().abs() < 0.01,
    "expected BFC root block to stay at x=0, got x={:.2}",
    bfc_frag.bounds.x()
  );
  assert!(
    (bfc_frag.bounds.y() - 20.0).abs() < 0.01,
    "expected BFC root block to be pushed below the float to y=20, got y={:.2}",
    bfc_frag.bounds.y()
  );
}

#[test]
fn bfc_root_auto_width_shrinks_to_fit_next_to_floats() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(50.0));
  float_style.height = Some(Length::px(20.0));
  let float_node =
    BoxNode::new_inline_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  // `overflow:hidden` establishes a BFC. When `width:auto`, the used width should shrink to the
  // remaining available width next to floats instead of being forced below them.
  let mut bfc_style = ComputedStyle::default();
  bfc_style.display = Display::Block;
  bfc_style.overflow_x = Overflow::Hidden;
  bfc_style.overflow_y = Overflow::Hidden;
  bfc_style.height = Some(Length::px(10.0));
  let bfc_node = BoxNode::new_block(Arc::new(bfc_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![float_node, bfc_node],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  let bfc_frags: Vec<_> = fragment
    .children
    .iter()
    .filter(|child| (child.bounds.width() - 150.0).abs() < 0.01 && (child.bounds.height() - 10.0).abs() < 0.01)
    .collect();
  assert_eq!(
    bfc_frags.len(),
    1,
    "expected a single BFC root fragment; got {} children",
    fragment.children.len()
  );
  let bfc_frag = bfc_frags[0];
  assert!(
    (bfc_frag.bounds.x() - 50.0).abs() < 0.01,
    "expected BFC root block to start to the right of the float at x=50, got x={:.2}",
    bfc_frag.bounds.x()
  );
  assert!(
    bfc_frag.bounds.y().abs() < 0.01,
    "expected BFC root block to stay on the same line as the float at y=0, got y={:.2}",
    bfc_frag.bounds.y()
  );
}

#[test]
fn bfc_root_negative_margins_do_not_get_clamped_when_no_floats_overlap() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  // Flex containers establish a BFC. Bootstrap-style gutters use negative margins on these boxes to
  // extend them outside the containing block. This must work even when there are no floats in the
  // float context (an empty float context still exists during block layout).
  let mut bfc_style = ComputedStyle::default();
  bfc_style.display = Display::Flex;
  bfc_style.margin_left = Some(Length::px(-10.0));
  bfc_style.margin_right = Some(Length::px(-10.0));
  bfc_style.height = Some(Length::px(10.0));
  let bfc_node = BoxNode::new_block(Arc::new(bfc_style), FormattingContextType::Flex, vec![]);

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![bfc_node],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  assert_eq!(
    fragment.children.len(),
    1,
    "expected a single child fragment, got {}",
    fragment.children.len()
  );
  let child = &fragment.children[0];
  assert!(
    (child.bounds.x() - (-10.0)).abs() < 0.01,
    "expected negative margin-left to shift the BFC root to x=-10, got x={:.2}",
    child.bounds.x()
  );
}

#[test]
fn bfc_root_float_avoidance_accounts_for_offset_containing_block_in_shared_float_context() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  // Create a nested block that does **not** establish a new BFC, but is horizontally offset.
  // Because it does not establish a BFC, it reuses the ancestor float context; its containing
  // block left edge inside that shared float context is non-zero.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.margin_left = Some(Length::px(8.0));

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(50.0));
  float_style.height = Some(Length::px(20.0));
  let float_node =
    BoxNode::new_inline_block(Arc::new(float_style), FormattingContextType::Block, vec![]);

  // `display: table` establishes a new BFC, so its border box must avoid overlap with float
  // margin boxes in the same (shared) float context.
  let mut bfc_style = ComputedStyle::default();
  bfc_style.display = Display::Table;
  bfc_style.width = Some(Length::px(60.0));
  bfc_style.height = Some(Length::px(10.0));
  let bfc_node = BoxNode::new_block(Arc::new(bfc_style), FormattingContextType::Table, vec![]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![float_node, bfc_node],
  );
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![container],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  assert_eq!(
    fragment.children.len(),
    1,
    "expected a single container child fragment, got {}",
    fragment.children.len()
  );
  let container_frag = &fragment.children[0];

  let bfc_frags: Vec<_> = container_frag
    .children
    .iter()
    .filter(|child| {
      (child.bounds.width() - 60.0).abs() < 0.01 && (child.bounds.height() - 10.0).abs() < 0.01
    })
    .collect();
  assert_eq!(
    bfc_frags.len(),
    1,
    "expected a single BFC root fragment inside the container; got {} children",
    container_frag.children.len()
  );
  let bfc_frag = bfc_frags[0];
  assert!(
    (bfc_frag.bounds.x() - 50.0).abs() < 0.01,
    "expected BFC root to start at x=50 (float width) relative to the container, got x={:.2}",
    bfc_frag.bounds.x()
  );
}
