use fastrender::geometry::Size;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{BoxSizing, JustifyContent};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::FragmentNode;
use std::sync::Arc;

fn fragment_box_id(fragment: &FragmentNode) -> Option<usize> {
  match &fragment.content {
    FragmentContent::Block { box_id }
    | FragmentContent::Inline { box_id, .. }
    | FragmentContent::Replaced { box_id, .. }
    | FragmentContent::Text { box_id, .. } => *box_id,
    FragmentContent::Line { .. }
    | FragmentContent::RunningAnchor { .. }
    | FragmentContent::FootnoteAnchor { .. } => None,
  }
}

fn assert_flex_root_max_width_case(
  mut style: ComputedStyle,
  max_width: Length,
  expected_root_border_box_width: f32,
  expected_child_x: f32,
) {
  let fc = FlexFormattingContext::with_viewport(Size::new(800.0, 600.0));

  style.display = Display::Flex;
  style.justify_content = JustifyContent::FlexEnd;
  style.max_width = Some(max_width);
  style.max_width_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(20.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(
    Arc::new(child_style),
    FormattingContextType::Block,
    Vec::new(),
  );
  child.id = 1;

  let container = BoxNode::new_block(
    Arc::new(style),
    FormattingContextType::Flex,
    vec![child],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(300.0))
    .expect("layout should succeed");

  assert!(
    (fragment.bounds.width() - expected_root_border_box_width).abs() < 0.5,
    "expected max-width to clamp fill-available width:auto to {expected_root_border_box_width:.1}px (got {:.1})",
    fragment.bounds.width()
  );

  // Ensure we don't correct the root size without rerunning flex layout, which would cause children
  // to be positioned as though the container were still 300px wide.
  let child_fragment = fragment
    .children
    .iter()
    .find(|f| fragment_box_id(f) == Some(1))
    .expect("expected flex child with id=1");
  assert!(
    (child_fragment.bounds.x() - expected_child_x).abs() < 0.5,
    "expected justify-content:flex-end child to be placed at x={expected_child_x:.1}px (got {:.1}, root_w={:.1})",
    child_fragment.bounds.x(),
    fragment.bounds.width()
  );
  assert!(
    child_fragment.bounds.max_x() <= fragment.bounds.width() + 0.5,
    "expected child to remain within the clamped root width (child_bounds={:?}, root_bounds={:?})",
    child_fragment.bounds,
    fragment.bounds
  );
}

#[test]
fn flex_root_auto_width_fill_available_respects_max_width() {
  let style = ComputedStyle::default();
  assert_flex_root_max_width_case(
    style,
    Length::px(150.0),
    /* expected_root_border_box_width */ 150.0,
    /* expected_child_x */ 130.0,
  );
}

#[test]
fn flex_root_auto_width_fill_available_respects_max_width_with_padding_content_box() {
  let mut style = ComputedStyle::default();
  // Default is `box-sizing: content-box`.
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // max-width: 150px constrains the *content* width, so the border-box width must include padding.
  assert_flex_root_max_width_case(
    style,
    Length::px(150.0),
    /* expected_root_border_box_width */ 170.0,
    /* expected_child_x */ 140.0,
  );
}

#[test]
fn flex_root_auto_width_fill_available_respects_max_width_with_padding_border_box() {
  let mut style = ComputedStyle::default();
  style.box_sizing = BoxSizing::BorderBox;
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // max-width: 150px constrains the border box, so the content box shrinks by padding.
  assert_flex_root_max_width_case(
    style,
    Length::px(150.0),
    /* expected_root_border_box_width */ 150.0,
    /* expected_child_x */ 120.0,
  );
}

#[test]
fn flex_root_auto_width_fill_available_respects_max_width_percent() {
  let style = ComputedStyle::default();
  assert_flex_root_max_width_case(
    style,
    Length::percent(50.0),
    /* expected_root_border_box_width */ 150.0,
    /* expected_child_x */ 130.0,
  );
}

#[test]
fn flex_root_auto_width_fill_available_respects_max_width_percent_with_padding_content_box() {
  let mut style = ComputedStyle::default();
  // Default is `box-sizing: content-box`.
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // max-width: 50% constrains the *content* width (50% of 300px), so padding expands the border-box width.
  assert_flex_root_max_width_case(
    style,
    Length::percent(50.0),
    /* expected_root_border_box_width */ 170.0,
    /* expected_child_x */ 140.0,
  );
}

#[test]
fn flex_root_auto_width_fill_available_respects_max_width_percent_with_padding_border_box() {
  let mut style = ComputedStyle::default();
  style.box_sizing = BoxSizing::BorderBox;
  style.padding_left = Length::px(10.0);
  style.padding_right = Length::px(10.0);
  // max-width: 50% constrains the border box, so the content box shrinks by padding.
  assert_flex_root_max_width_case(
    style,
    Length::percent(50.0),
    /* expected_root_border_box_width */ 150.0,
    /* expected_child_x */ 120.0,
  );
}
