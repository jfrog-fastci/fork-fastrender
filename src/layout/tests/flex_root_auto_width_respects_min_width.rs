use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{BoxSizing, FlexDirection, JustifyContent};
use fastrender::style::values::Length;
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

fn assert_flex_root_min_width_case(
  mut container_style: ComputedStyle,
  min_width: Length,
  expected_root_border_box_width: f32,
  expected_child_1_x: f32,
  expected_child_2_x: f32,
) {
  let fc = FlexFormattingContext::new();

  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = None;
  container_style.width_keyword = None;
  container_style.min_width = Some(min_width);
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
    (fragment.bounds.width() - expected_root_border_box_width).abs() < eps,
    "expected flex container to clamp up to min-width {expected_root_border_box_width}, got {}",
    fragment.bounds.width()
  );
  assert_eq!(
    fragment.children.len(),
    2,
    "expected flex container to have 2 children fragments"
  );

  assert!(
    (fragment.children[0].bounds.x() - expected_child_1_x).abs() < eps,
    "expected first child to be placed at x={expected_child_1_x}, got x={}",
    fragment.children[0].bounds.x()
  );
  assert!(
    (fragment.children[1].bounds.x() - expected_child_2_x).abs() < eps,
    "expected second child to be placed after the first at x={expected_child_2_x}, got x={}",
    fragment.children[1].bounds.x()
  );
}

#[test]
fn flex_root_auto_width_respects_min_width() {
  let style = ComputedStyle::default();
  assert_flex_root_min_width_case(
    style,
    Length::px(400.0),
    /* expected_root_border_box_width */ 400.0,
    /* expected_child_1_x */ 300.0,
    /* expected_child_2_x */ 350.0,
  );
}

#[test]
fn flex_root_auto_width_respects_min_width_with_padding_content_box() {
  let mut style = ComputedStyle::default();
  // Default is `box-sizing: content-box`.
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // min-width: 400px constrains the *content* width, so padding expands the border-box width.
  assert_flex_root_min_width_case(
    style,
    Length::px(400.0),
    /* expected_root_border_box_width */ 420.0,
    /* expected_child_1_x */ 310.0,
    /* expected_child_2_x */ 360.0,
  );
}

#[test]
fn flex_root_auto_width_respects_min_width_with_padding_border_box() {
  let mut style = ComputedStyle::default();
  style.box_sizing = BoxSizing::BorderBox;
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // min-width: 400px constrains the border box, so the content box shrinks by padding.
  assert_flex_root_min_width_case(
    style,
    Length::px(400.0),
    /* expected_root_border_box_width */ 400.0,
    /* expected_child_1_x */ 290.0,
    /* expected_child_2_x */ 340.0,
  );
}

#[test]
fn flex_root_auto_width_respects_min_width_percent() {
  let style = ComputedStyle::default();
  // Percentage min-width resolves against the available width (200px).
  assert_flex_root_min_width_case(
    style,
    Length::percent(150.0),
    /* expected_root_border_box_width */ 300.0,
    /* expected_child_1_x */ 200.0,
    /* expected_child_2_x */ 250.0,
  );
}

#[test]
fn flex_root_auto_width_respects_min_width_percent_with_padding_content_box() {
  let mut style = ComputedStyle::default();
  // Default is `box-sizing: content-box`.
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // min-width: 150% constrains the *content* width, so padding expands the border box.
  assert_flex_root_min_width_case(
    style,
    Length::percent(150.0),
    /* expected_root_border_box_width */ 320.0,
    /* expected_child_1_x */ 210.0,
    /* expected_child_2_x */ 260.0,
  );
}

#[test]
fn flex_root_auto_width_respects_min_width_percent_with_padding_border_box() {
  let mut style = ComputedStyle::default();
  style.box_sizing = BoxSizing::BorderBox;
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // min-width: 150% constrains the border box, so the content box shrinks by padding.
  assert_flex_root_min_width_case(
    style,
    Length::percent(150.0),
    /* expected_root_border_box_width */ 300.0,
    /* expected_child_1_x */ 190.0,
    /* expected_child_2_x */ 240.0,
  );
}
