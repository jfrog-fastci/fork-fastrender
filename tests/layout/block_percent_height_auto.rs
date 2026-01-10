use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn percent_height_in_auto_height_block_container_computes_to_auto() {
  // CSS2.1 §10.5: Percentage `height` values compute to `auto` when the containing block's height
  // is not specified explicitly. Block layout frequently runs with a definite available height
  // (e.g. the viewport), but that is not a valid basis for resolving `height:100%` when the
  // containing block itself is `height:auto`.

  let mut fixed_child_style = ComputedStyle::default();
  fixed_child_style.display = Display::Block;
  fixed_child_style.height = Some(Length::px(10.0));
  fixed_child_style.height_keyword = None;
  let fixed_child =
    BoxNode::new_block(Arc::new(fixed_child_style), FormattingContextType::Block, vec![]);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  flex_style.height = Some(Length::percent(100.0));
  flex_style.height_keyword = None;
  let flex_box = BoxNode::new_block(Arc::new(flex_style), FormattingContextType::Flex, vec![fixed_child]);

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Block;
  let parent = BoxNode::new_block(Arc::new(parent_style), FormattingContextType::Block, vec![flex_box]);

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(
      &parent,
      &LayoutConstraints::new(
        AvailableSpace::Definite(100.0),
        AvailableSpace::Definite(100.0),
      ),
    )
    .expect("layout should succeed");

  let flex_fragment = fragment.children.first().expect("flex fragment");
  assert!(
    (flex_fragment.bounds.height() - 10.0).abs() < 0.5,
    "expected `height:100%` to compute to `auto` when containing block height is auto (got {})",
    flex_fragment.bounds.height()
  );
}

