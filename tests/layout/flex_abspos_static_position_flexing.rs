use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::FlexBasis;
use fastrender::style::types::JustifyContent;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::FragmentNode;
use std::sync::Arc;

fn abspos_x(fragment: &FragmentNode) -> f32 {
  fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Absolute)))
    .expect("absolute fragment present")
    .bounds
    .x()
}

fn flex_container(justify: JustifyContent, children: Vec<BoxNode>) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Flex;
  style.position = Position::Relative;
  style.width = Some(Length::px(100.0));
  style.height = Some(Length::px(40.0));
  style.justify_content = justify;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Flex, children)
}

#[test]
fn abspos_static_position_does_not_flex_child_size() {
  let fc = FlexFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 40.0);

  // The abspos child is not a flex item, but it can still have `flex` properties set. The flexbox
  // spec says to compute the static position as if it were the sole flex item *with a fixed used
  // size*, so flexing must not change the size used for alignment.
  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.flex_grow = 1.0;
  abs_style.flex_shrink = 1.0;
  abs_style.flex_basis = FlexBasis::Length(Length::percent(0.0)); // typical of `flex: 1`
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);

  let container = flex_container(JustifyContent::Center, vec![abs_child]);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let expected = 45.0; // (100 - 10) / 2
  assert!(
    (abspos_x(&fragment) - expected).abs() < 0.1,
    "expected centered abspos x≈{expected}px, got {}",
    abspos_x(&fragment)
  );
}

