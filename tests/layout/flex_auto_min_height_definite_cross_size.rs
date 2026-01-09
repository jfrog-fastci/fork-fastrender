use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{FlexDirection, Overflow};
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn build_wrapping_text_item(item_id: usize, text_id: usize, repeat: usize) -> BoxNode {
  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  let text_style = Arc::new(text_style);

  let text = "word ".repeat(repeat);
  let mut text_node = BoxNode::new_text(text_style, text);
  text_node.id = text_id;

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.overflow_x = Overflow::Visible;
  item_style.overflow_y = Overflow::Visible;
  let item_style = Arc::new(item_style);

  let mut item = BoxNode::new_block(item_style, FormattingContextType::Block, vec![text_node]);
  item.id = item_id;
  item
}

#[test]
fn flex_auto_min_height_uses_container_cross_size_when_definite() {
  // Regression test: `min-height:auto` for column flex items should use the item's actual
  // (definite) cross size when computing the content-based minimum height. Measuring at
  // min-content width dramatically overestimates heights for multi-line text, which then forces
  // flex items to an inflated used height.
  let container_width = 200.0;
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(container_width),
    AvailableSpace::Indefinite,
  );

  let item_for_block = build_wrapping_text_item(2, 3, 200);
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
  let block_item_fragment = block_fragment
    .children
    .first()
    .expect("expected block item fragment");
  let expected_height = block_item_fragment.bounds.height();

  let item_for_flex = build_wrapping_text_item(12, 13, 200);
  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  let mut flex_container = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![item_for_flex],
  );
  flex_container.id = 11;

  let flex_fc = FlexFormattingContext::new();
  let flex_fragment = flex_fc
    .layout(&flex_container, &constraints)
    .expect("flex layout should succeed");
  let flex_item_fragment = flex_fragment
    .children
    .first()
    .expect("expected flex item fragment");
  let flex_height = flex_item_fragment.bounds.height();

  assert!(
    expected_height.is_finite() && expected_height > 0.0,
    "expected block layout to produce a finite non-zero height (got {expected_height})"
  );
  assert!(
    (flex_height - expected_height).abs() < 0.5,
    "expected column flex item height to match block layout at the same definite width; got flex={flex_height:.2}, block={expected_height:.2}"
  );
}

