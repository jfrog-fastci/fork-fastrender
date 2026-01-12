use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::BoxTree;
use std::sync::Arc;

#[test]
fn flex_empty_anonymous_block_item_does_not_expand_to_container_width() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));

  let mut left_style = ComputedStyle::default();
  left_style.display = Display::Block;
  left_style.width = Some(Length::px(100.0));
  // Equivalent to `margin-right: auto` so remaining items are pushed to the end.
  left_style.margin_right = None;

  let mut right_style = ComputedStyle::default();
  right_style.display = Display::Block;
  right_style.width = Some(Length::px(40.0));

  let mut anonymous_style = ComputedStyle::default();
  anonymous_style.display = Display::Block;

  let mut empty_inline_style = ComputedStyle::default();
  empty_inline_style.display = Display::Inline;

  let left = BoxNode::new_block(
    Arc::new(left_style),
    FormattingContextType::Block,
    Vec::new(),
  );

  let empty_inline = BoxNode::new_inline(Arc::new(empty_inline_style), Vec::new());
  let empty_anonymous = BoxNode::new_anonymous_block(Arc::new(anonymous_style), vec![empty_inline]);

  let right = BoxNode::new_block(
    Arc::new(right_style),
    FormattingContextType::Block,
    Vec::new(),
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![left, empty_anonymous, right],
  );
  let tree = BoxTree::new(container);

  let block_fc = BlockFormattingContext::new();
  let empty_anonymous_node = tree
    .root
    .children
    .get(1)
    .expect("anonymous flex child should exist");
  let intrinsic = block_fc
    .compute_intrinsic_inline_size(empty_anonymous_node, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic sizing should succeed");
  assert!(
    intrinsic <= 0.01,
    "expected empty anonymous block to have 0px max-content width (got {intrinsic})"
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &tree.root,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 3);
  let eps = 0.5;
  let container_width = fragment.bounds.width();
  assert!(
    (container_width - 200.0).abs() <= eps,
    "expected container to have definite width 200px (got {container_width})"
  );

  let anonymous_fragment = &fragment.children[1];
  let anonymous_width = anonymous_fragment.bounds.width();
  assert!(
    anonymous_width <= eps,
    "expected empty anonymous block flex item to measure ~0px wide (got {anonymous_width})"
  );

  let right_fragment = &fragment.children[2];
  let right_edge = right_fragment.bounds.x() + right_fragment.bounds.width();
  assert!(
    right_edge <= container_width + eps,
    "expected the final flex item to stay within the container (right edge {right_edge} > container {container_width})"
  );
  assert!(
    (right_edge - container_width).abs() <= eps,
    "expected margin-right:auto to push the last flex item flush to the end (right edge {right_edge}, container {container_width})"
  );
}
