use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
use fastrender::style::values::{Length, LengthUnit};
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::FragmentNode;
use std::sync::Arc;

fn find_abs_fragment<'a>(fragment: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if matches!(
    fragment.style.as_ref().map(|s| s.position),
    Some(Position::Absolute)
  ) {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_abs_fragment(child) {
      return Some(found);
    }
  }
  None
}

#[test]
fn abspos_percent_sizing_uses_flex_container_containing_block() {
  // Regression test for abspos percentage sizing inside a flex container.
  //
  // The containing block for absolutely positioned descendants should be the flex container's
  // padding box when the container establishes a positioned containing block. Percentage heights
  // should resolve against that block size, not to zero.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(80.0));

  let mut flex_item_style = ComputedStyle::default();
  flex_item_style.display = Display::Block;

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::new(100.0, LengthUnit::Percent));
  abs_style.height = Some(Length::new(100.0, LengthUnit::Percent));
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.right = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));
  abs_style.left = InsetValue::Length(Length::px(0.0));

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let flex_item = BoxNode::new_block(
    Arc::new(flex_item_style),
    FormattingContextType::Block,
    vec![abs_child],
  );
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![flex_item],
  );

  let constraints = LayoutConstraints::definite(100.0, 80.0);
  let fc = FlexFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("flex layout");
  let abs_fragment = find_abs_fragment(&fragment).expect("expected abspos fragment");

  assert!(
    (abs_fragment.bounds.width() - 100.0).abs() < 0.1,
    "expected abspos width≈100, got {}",
    abs_fragment.bounds.width()
  );
  assert!(
    (abs_fragment.bounds.height() - 80.0).abs() < 0.1,
    "expected abspos height≈80, got {}",
    abs_fragment.bounds.height()
  );
}

