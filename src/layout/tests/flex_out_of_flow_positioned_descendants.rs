use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::FlexDirection;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn flex_item_size_ignores_fixed_out_of_flow_descendants() {
  // Regression test for pages like vogue.com where hidden nav overlays are implemented as
  // `position: fixed` descendants inside an otherwise-empty in-flow wrapper.
  //
  // Out-of-flow positioned descendants must not affect the flex item's measured size; otherwise the
  // wrapper can incorrectly take up viewport height and push real content down.

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.width = Some(Length::px(200.0));

  let mut fixed_overlay_style = ComputedStyle::default();
  fixed_overlay_style.display = Display::Block;
  fixed_overlay_style.position = Position::Fixed;
  fixed_overlay_style.width = Some(Length::px(200.0));
  fixed_overlay_style.height = Some(Length::px(500.0));

  let fixed_overlay = BoxNode::new_block(
    Arc::new(fixed_overlay_style),
    FormattingContextType::Block,
    vec![],
  );

  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::Block;
  // Leave height as `auto` (unset) so the wrapper should be 0px tall when it has no in-flow content.

  let wrapper = BoxNode::new_block(
    Arc::new(wrapper_style),
    FormattingContextType::Block,
    vec![fixed_overlay],
  );

  let mut content_style = ComputedStyle::default();
  content_style.display = Display::Block;
  content_style.height = Some(Length::px(10.0));
  let content = BoxNode::new_block(
    Arc::new(content_style),
    FormattingContextType::Block,
    vec![],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![wrapper, content],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.height() - 10.0).abs() < 0.1,
    "flex container height should ignore fixed overlay (got {})",
    fragment.bounds.height()
  );

  let wrapper_fragment = fragment.children.get(0).expect("wrapper fragment");
  assert!(
    wrapper_fragment.bounds.height() <= 0.1,
    "wrapper should remain 0px tall (got {})",
    wrapper_fragment.bounds.height()
  );

  let content_fragment = fragment.children.get(1).expect("content fragment");
  assert!(
    content_fragment.bounds.y().abs() < 0.1,
    "content should not be pushed down by out-of-flow overlay (y={})",
    content_fragment.bounds.y()
  );
}

#[test]
fn flex_item_size_ignores_out_of_flow_descendants_nested_in_empty_in_flow_wrappers() {
  // Regression test for pages like yahoo.com where a flex item contains a list wrapper with an
  // in-flow `<li>` but only out-of-flow (e.g. `position: absolute`) content. The wrapper should not
  // expand to the viewport width during intrinsic sizing and push later flex items off-screen.

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.width = Some(Length::px(500.0));

  let mut abspos_style = ComputedStyle::default();
  abspos_style.display = Display::Block;
  abspos_style.position = Position::Absolute;
  abspos_style.width = Some(Length::px(100.0));
  abspos_style.height = Some(Length::px(10.0));
  let abspos_child =
    BoxNode::new_block(Arc::new(abspos_style), FormattingContextType::Block, vec![]);

  let mut li_style = ComputedStyle::default();
  li_style.display = Display::Block;
  let li_wrapper = BoxNode::new_block(
    Arc::new(li_style),
    FormattingContextType::Block,
    vec![abspos_child],
  );

  let mut ul_style = ComputedStyle::default();
  ul_style.display = Display::Block;
  let ul_wrapper = BoxNode::new_block(
    Arc::new(ul_style),
    FormattingContextType::Block,
    vec![li_wrapper],
  );

  let mut content_style = ComputedStyle::default();
  content_style.display = Display::Block;
  content_style.width = Some(Length::px(50.0));
  content_style.height = Some(Length::px(20.0));
  let content = BoxNode::new_block(
    Arc::new(content_style),
    FormattingContextType::Block,
    vec![],
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![ul_wrapper, content],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(500.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  let ul_fragment = fragment.children.get(0).expect("ul fragment");
  assert!(
    ul_fragment.bounds.width() <= 0.1,
    "wrapper should remain 0px wide (got {})",
    ul_fragment.bounds.width()
  );

  let content_fragment = fragment.children.get(1).expect("content fragment");
  assert!(
    content_fragment.bounds.x().abs() < 0.1,
    "content should not be pushed right by out-of-flow wrapper (x={})",
    content_fragment.bounds.x()
  );
}
