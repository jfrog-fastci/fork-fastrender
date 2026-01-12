use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::position::Position;
use crate::style::types::BorderStyle;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn abspos_static_position_is_rebased_when_flex_container_uses_inherited_containing_block() {
  // Regression test for abspos static-position calculation in flex layout:
  //
  // When a flex container does *not* establish an absolute-position containing block, its abspos
  // children are positioned against an inherited containing block (often the viewport or an
  // ancestor padding box). The static position for abspos children is computed in the flex
  // container's coordinate space, but the absolute positioning algorithm expects it to be in the
  // containing block's coordinate space.
  //
  // Previously we passed the container-local static position directly, which caused abspos
  // children with all insets `auto` (common for `::before` dividers) to be offset by the negative
  // container placement and end up at (0,0) in the global coordinate space.
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.width = Some(Length::px(200.0));
  root_style.height = Some(Length::px(200.0));
  // Ensure the first child's top margin does not collapse with the root so the flex container is
  // actually offset within the containing block.
  root_style.border_top_width = Length::px(1.0);
  root_style.border_top_style = BorderStyle::Solid;

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.width = Some(Length::px(100.0));
  flex_style.height = Some(Length::px(50.0));
  flex_style.margin_left = Some(Length::px(50.0));
  flex_style.margin_top = Some(Length::px(40.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let flex = BoxNode::new_block(
    Arc::new(flex_style),
    FormattingContextType::Flex,
    vec![abs_child],
  );
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![flex],
  );

  let constraints = LayoutConstraints::definite(200.0, 200.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  assert_eq!(
    fragment.children.len(),
    1,
    "flex container fragment should be present"
  );
  let flex_fragment = &fragment.children[0];

  let abs_fragment = flex_fragment
    .children
    .iter()
    .find(|child| {
      matches!(
        child.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute-positioned child fragment should exist");

  assert!(
    abs_fragment.bounds.x().abs() < 0.1 && abs_fragment.bounds.y().abs() < 0.1,
    "abspos child with auto insets should use its static position within the flex container (got ({}, {}))",
    abs_fragment.bounds.x(),
    abs_fragment.bounds.y()
  );
}
