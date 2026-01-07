use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
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
fn abspos_static_position_uses_margin_edges_on_main_axis() {
  let fc = FlexFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 40.0);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.margin_left = Some(Length::px(20.0));
  abs_style.margin_right = Some(Length::px(0.0));
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);

  let container = flex_container(JustifyContent::FlexStart, vec![abs_child]);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  assert!(
    (abspos_x(&fragment) - 20.0).abs() < 0.1,
    "expected abspos border box x≈20px (margin edge at 0), got {}",
    abspos_x(&fragment)
  );
}

#[test]
fn abspos_static_position_treats_auto_margins_as_zero() {
  let fc = FlexFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 40.0);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.width = Some(Length::px(10.0));
  abs_style.height = Some(Length::px(10.0));
  abs_style.margin_left = None; // auto
  abs_style.margin_right = None; // auto
  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);

  let container = flex_container(JustifyContent::Center, vec![abs_child]);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let expected = 45.0; // (100 - 10) / 2
  assert!(
    (abspos_x(&fragment) - expected).abs() < 0.1,
    "expected abspos border box x≈{expected}px when auto margins are treated as 0, got {}",
    abspos_x(&fragment)
  );
}

