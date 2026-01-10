use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{FlexDirection, JustifyContent};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

#[test]
fn flex_root_auto_width_respects_min_width() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = None;
  container_style.width_keyword = None;
  container_style.min_width = Some(Length::px(400.0));
  container_style.min_width_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(50.0));
  child_style.height = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height_keyword = None;
  child_style.flex_shrink = 0.0;

  let mut child_1 = BoxNode::new_block(
    Arc::new(child_style.clone()),
    FormattingContextType::Block,
    vec![],
  );
  child_1.id = 1;

  let mut child_2 = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child_2.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_1, child_2],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(200.0))
    .expect("layout succeeds");

  let eps = 1e-3;
  assert!(
    (fragment.bounds.width() - 400.0).abs() < eps,
    "expected flex container to clamp up to min-width, got {}",
    fragment.bounds.width()
  );
  assert_eq!(
    fragment.children.len(),
    2,
    "expected flex container to have 2 children fragments"
  );

  assert!(
    (fragment.children[0].bounds.x() - 300.0).abs() < eps,
    "expected first child to be placed at the end of the 400px container, got x={}",
    fragment.children[0].bounds.x()
  );
  assert!(
    (fragment.children[1].bounds.x() - 350.0).abs() < eps,
    "expected second child to be placed after the first, got x={}",
    fragment.children[1].bounds.x()
  );
}

