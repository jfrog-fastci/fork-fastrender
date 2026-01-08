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
use std::sync::Arc;

fn abspos_child(order: i32) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.position = Position::Absolute;
  style.width = Some(Length::px(10.0));
  style.height = Some(Length::px(10.0));
  style.order = order;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

fn abspos_child_with_width(order: i32, width: f32) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.position = Position::Absolute;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(10.0));
  style.order = order;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

fn inflow_child() -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(20.0));
  style.height = Some(Length::px(10.0));
  style.flex_shrink = 0.0;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
}

fn flex_container(children: Vec<BoxNode>) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Flex;
  style.position = Position::Relative;
  style.width = Some(Length::px(100.0));
  style.height = Some(Length::px(40.0));
  style.justify_content = JustifyContent::Center;
  BoxNode::new_block(Arc::new(style), FormattingContextType::Flex, children)
}

fn abspos_x(fragment: &fastrender::tree::fragment_tree::FragmentNode) -> f32 {
  fragment
    .children
    .iter()
    .find(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Absolute)))
    .expect("absolute fragment present")
    .bounds
    .x()
}

fn abspos_x_by_order(fragment: &fastrender::tree::fragment_tree::FragmentNode, order: i32) -> f32 {
  fragment
    .children
    .iter()
    .filter(|child| matches!(child.style.as_ref().map(|s| s.position), Some(Position::Absolute)))
    .find(|child| child.style.as_ref().is_some_and(|s| s.order == order))
    .unwrap_or_else(|| {
      let debug_orders: Vec<i32> = fragment
        .children
        .iter()
        .filter_map(|child| {
          child
            .style
            .as_ref()
            .filter(|s| matches!(s.position, Position::Absolute))
            .map(|s| s.order)
        })
        .collect();
      panic!("absolute fragment with order {order} present; saw orders={debug_orders:?}");
    })
    .bounds
    .x()
}

#[test]
fn abspos_static_position_is_independent_of_siblings_and_order() {
  let fc = FlexFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 40.0);

  // A) Sole abspos child.
  let container_a = flex_container(vec![abspos_child(0)]);
  let frag_a = fc.layout(&container_a, &constraints).expect("layout A");
  let x_a = abspos_x(&frag_a);

  // B) Same abspos child plus an in-flow flex item (abspos first in DOM order).
  let container_b = flex_container(vec![abspos_child(0), inflow_child()]);
  let frag_b = fc.layout(&container_b, &constraints).expect("layout B");
  let x_b = abspos_x(&frag_b);

  // C) Same as B but change the abspos child's `order`.
  let container_c = flex_container(vec![abspos_child(10), inflow_child()]);
  let frag_c = fc.layout(&container_c, &constraints).expect("layout C");
  let x_c = abspos_x(&frag_c);

  let expected_center = 45.0; // (100 - 10) / 2
  let eps = 0.1;

  assert!(
    (x_a - expected_center).abs() < eps,
    "expected centered abspos x ≈ {expected_center}, got {x_a}"
  );
  assert!(
    (x_a - x_b).abs() < eps,
    "abspos static x must be independent of siblings: A={x_a} B={x_b}"
  );
  assert!(
    (x_a - x_c).abs() < eps,
    "abspos static x must ignore `order`: A={x_a} C={x_c}"
  );
}

#[test]
fn abspos_static_position_is_independent_of_abspos_siblings() {
  let fc = FlexFormattingContext::new();
  let constraints = LayoutConstraints::definite(100.0, 40.0);

  let container = flex_container(vec![
    abspos_child_with_width(1, 10.0),
    abspos_child_with_width(2, 50.0),
  ]);
  let fragment = fc.layout(&container, &constraints).expect("layout");

  let x_small = abspos_x_by_order(&fragment, 1);
  let x_large = abspos_x_by_order(&fragment, 2);

  assert!(
    (x_small - 45.0).abs() < 0.1,
    "expected 10px abspos child to center at x≈45, got {x_small}"
  );
  assert!(
    (x_large - 25.0).abs() < 0.1,
    "expected 50px abspos child to center at x≈25, got {x_large}"
  );
}
