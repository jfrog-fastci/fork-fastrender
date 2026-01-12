use std::sync::Arc;

use crate::layout::constraints::AvailableSpace;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::types::AlignItems;
use crate::style::types::FlexDirection;
use crate::style::values::Length;
use crate::tree::box_tree::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContext;

#[test]
fn flex_intrinsic_width_probe_preserves_nested_flex_item_block_size() {
  // Regression test for intrinsic-width measurement of flex items (e.g. when the container uses
  // `align-items: center`). The intrinsic-width fast-path must still report a non-zero block-size
  // for nested flex/grid containers; otherwise the parent flex container can treat the item as
  // 0px tall and overlap subsequent siblings.

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Flex;
  outer_style.flex_direction = FlexDirection::Column;
  outer_style.align_items = AlignItems::Center;
  outer_style.width = Some(Length::px(200.0));

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Flex;

  let mut inner_child_style = ComputedStyle::default();
  inner_child_style.display = Display::Block;
  inner_child_style.width = Some(Length::px(100.0));
  inner_child_style.height = Some(Length::px(50.0));

  let inner_child = BoxNode::new_block(
    Arc::new(inner_child_style),
    FormattingContextType::Block,
    vec![],
  );

  let inner = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Flex,
    vec![inner_child],
  );

  let mut sibling_style = ComputedStyle::default();
  sibling_style.display = Display::Block;
  sibling_style.width = Some(Length::px(100.0));
  sibling_style.height = Some(Length::px(10.0));
  let sibling = BoxNode::new_block(
    Arc::new(sibling_style),
    FormattingContextType::Block,
    vec![],
  );

  let outer = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Flex,
    vec![inner, sibling],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &outer,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let inner_fragment = &fragment.children[0];
  let sibling_fragment = &fragment.children[1];

  assert!(
    inner_fragment.bounds.height() > 49.0,
    "expected nested flex item to have non-zero height, got {}",
    inner_fragment.bounds.height()
  );
  assert!(
    sibling_fragment.bounds.y() >= inner_fragment.bounds.height() - 0.5,
    "expected sibling to be placed after nested flex item; inner_height={} sibling_y={}",
    inner_fragment.bounds.height(),
    sibling_fragment.bounds.y()
  );
}
