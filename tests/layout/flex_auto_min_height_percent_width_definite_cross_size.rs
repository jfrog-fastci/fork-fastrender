use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{FlexDirection, Overflow};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn build_aspect_ratio_item(item_id: usize, ratio_child_id: usize) -> BoxNode {
  // Mimic the common "intrinsic ratio" pattern used by many sites: a zero-content box whose height
  // is created by percentage padding, so its block size depends on its resolved width.
  let mut ratio_style = ComputedStyle::default();
  ratio_style.display = Display::Block;
  ratio_style.padding_top = Length::percent(56.25);
  let ratio_style = Arc::new(ratio_style);

  let mut ratio_child = BoxNode::new_block(ratio_style, FormattingContextType::Block, Vec::new());
  ratio_child.id = ratio_child_id;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.width = Some(Length::percent(100.0));
  item_style.overflow_x = Overflow::Visible;
  item_style.overflow_y = Overflow::Visible;
  let item_style = Arc::new(item_style);

  let mut item = BoxNode::new_block(item_style, FormattingContextType::Block, vec![ratio_child]);
  item.id = item_id;
  item
}

#[test]
fn flex_auto_min_height_percent_width_uses_container_cross_size() {
  // Regression test: `min-height:auto` for column flex items must compute the content-based
  // minimum height using the *resolved* cross size when that cross size depends on the container
  // (e.g. `width:100%`). If the intrinsic probe falls back to the viewport width as the percentage
  // base, it can dramatically inflate the computed min-height and force the flex item to an
  // incorrect used height (seen on youtube.com "thumbnail skeleton" cards).
  let container_width = 200.0;
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(container_width),
    AvailableSpace::Indefinite,
  );

  // Establish the expected content height by laying out the same subtree in normal block layout at
  // the same definite width.
  let item_for_block = build_aspect_ratio_item(2, 3);
  let mut block_root_style = ComputedStyle::default();
  block_root_style.display = Display::Block;
  let mut block_root = BoxNode::new_block(
    Arc::new(block_root_style),
    FormattingContextType::Block,
    vec![item_for_block],
  );
  block_root.id = 1;
  let block_fc = BlockFormattingContext::new();
  let block_fragment = block_fc
    .layout(&block_root, &constraints)
    .expect("block layout should succeed");
  let expected_height = block_fragment
    .children
    .first()
    .expect("expected block item fragment")
    .bounds
    .height();

  let item_for_flex = build_aspect_ratio_item(12, 13);
  let mut details_style = ComputedStyle::default();
  details_style.display = Display::Block;
  details_style.height = Some(Length::px(10.0));
  let mut details = BoxNode::new_block(Arc::new(details_style), FormattingContextType::Block, Vec::new());
  details.id = 14;

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  let mut flex_container = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![item_for_flex, details],
  );
  flex_container.id = 11;

  let flex_fc = FlexFormattingContext::new();
  let flex_fragment = flex_fc
    .layout(&flex_container, &constraints)
    .expect("flex layout should succeed");

  assert_eq!(
    flex_fragment.children.len(),
    2,
    "expected two flex items in fragment tree"
  );
  let flex_item_fragment = &flex_fragment.children[0];
  let flex_details_fragment = &flex_fragment.children[1];
  let flex_height = flex_item_fragment.bounds.height();

  assert!(
    expected_height.is_finite() && expected_height > 0.0,
    "expected block layout to produce a finite non-zero height (got {expected_height})"
  );
  assert!(
    (flex_height - expected_height).abs() < 1.0,
    "expected column flex item height to match block layout at the same definite width; got flex={flex_height:.2}, block={expected_height:.2}"
  );
  assert!(
    (flex_details_fragment.bounds.y() - expected_height).abs() < 1.0,
    "expected next flex item to be positioned immediately after the aspect-ratio item; got details_y={:.2}, expected={expected_height:.2}",
    flex_details_fragment.bounds.y()
  );
}
