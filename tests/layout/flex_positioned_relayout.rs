use fastrender::geometry::Size;
use fastrender::layout::constraints::{AvailableSpace, LayoutConstraints};
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::{FlexDirection, InsetValue, JustifyContent};
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use std::sync::Arc;

fn positioned_child(position: Position, left: f32, right: f32, top: f32, text: &str) -> BoxNode {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.position = position;
  style.left = InsetValue::Length(Length::px(left));
  style.right = InsetValue::Length(Length::px(right));
  style.top = InsetValue::Length(Length::px(top));

  let text_node = BoxNode::new_text(Arc::new(ComputedStyle::default()), text.to_string());
  BoxNode::new_block(
    Arc::new(style),
    FormattingContextType::Block,
    vec![text_node],
  )
}

fn descendant_max_right(fragment: &FragmentNode) -> f32 {
  let origin_x = fragment.bounds.x();
  fragment
    .iter_fragments()
    .map(|f| f.bounds.max_x() - origin_x)
    .fold(0.0, f32::max)
}

fn line_count(fragment: &FragmentNode) -> usize {
  fragment
    .iter_fragments()
    .filter(|f| matches!(f.content, FragmentContent::Line { .. }))
    .count()
}

#[test]
fn flex_positioned_children_relayout_is_stable() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(220.0));
  container_style.height = Some(Length::px(160.0));

  let abs_child = positioned_child(
    Position::Absolute,
    30.0,
    90.0,
    0.0,
    "Positioned flex item that should wrap once the available width is recomputed with insets.",
  );
  let fixed_child = positioned_child(
    Position::Fixed,
    40.0,
    40.0,
    48.0,
    "Fixed positioned item that shares the flex caches while being remeasured after absolute sizing.",
  );

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![abs_child, fixed_child],
  );

  let fc = FlexFormattingContext::with_viewport(Size::new(260.0, 200.0));
  let constraints = LayoutConstraints::new(
    AvailableSpace::Definite(220.0),
    AvailableSpace::Definite(160.0),
  );

  let first = fc.layout(&container, &constraints).expect("first layout");
  let second = fc.layout(&container, &constraints).expect("second layout");

  assert_eq!(first.children.len(), 2);
  assert_eq!(second.children.len(), 2);

  let expected_abs_width = 100.0;
  let expected_fixed_width = 180.0;

  for (fragment, expected_width) in [
    (&first.children[0], expected_abs_width),
    (&first.children[1], expected_fixed_width),
  ] {
    let max_right = descendant_max_right(fragment);
    assert!(
      (fragment.bounds.width() - expected_width).abs() < 0.5,
      "positioned child should size against insets (expected {expected_width}, got {})",
      fragment.bounds.width()
    );
    assert!(
      max_right <= fragment.bounds.width() + 0.5,
      "descendants should be laid out for the used width (max_right={max_right}, width={})",
      fragment.bounds.width()
    );
    assert!(
      line_count(fragment) > 1,
      "positioned children should wrap when width shrinks (lines={})",
      line_count(fragment)
    );
  }

  for (first_child, second_child) in first.children.iter().zip(second.children.iter()) {
    assert!(
      (first_child.bounds.x() - second_child.bounds.x()).abs() < 0.1
        && (first_child.bounds.y() - second_child.bounds.y()).abs() < 0.1,
      "positions should be stable across relayouts"
    );
    assert!(
      (first_child.bounds.width() - second_child.bounds.width()).abs() < 0.1
        && (first_child.bounds.height() - second_child.bounds.height()).abs() < 0.1,
      "sizes should be stable across relayouts"
    );
    assert_eq!(
      line_count(first_child),
      line_count(second_child),
      "line breaking should stay consistent when caches are reused"
    );
  }
}

#[test]
fn flex_positioned_inset_stretch_child_relayout_applies_justify_content_end() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(100.0));

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Flex;
  abs_style.position = Position::Absolute;
  abs_style.left = InsetValue::Length(Length::px(0.0));
  abs_style.right = InsetValue::Length(Length::px(0.0));
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));
  abs_style.flex_direction = FlexDirection::Column;
  abs_style.justify_content = JustifyContent::End;
  abs_style.padding_top = Length::px(10.0);
  abs_style.padding_right = Length::px(10.0);
  abs_style.padding_bottom = Length::px(10.0);
  abs_style.padding_left = Length::px(10.0);

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.width = Some(Length::px(20.0));
  inner_style.height = Some(Length::px(20.0));
  inner_style.flex_shrink = 0.0;
  let mut inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);
  inner.id = 3;

  let mut abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Flex, vec![inner]);
  abs_child.id = 2;
  let abs_child_direct = abs_child.clone();

  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![abs_child],
  );
  container.id = 1;

  let fc = FlexFormattingContext::new();

  // Sanity: forcing a used border-box height on an auto-height flex container must still allow
  // justify-content:end to distribute free space.
  let direct_fragment = fc
    .layout(
      &abs_child_direct,
      &LayoutConstraints::definite(100.0, 100.0).with_used_border_box_size(Some(100.0), Some(100.0)),
    )
    .expect("direct layout succeeds");
  let direct_inner = direct_fragment
    .children
    .iter()
    .find(|fragment| {
      matches!(
        fragment.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == 3
      )
    })
    .unwrap_or_else(|| panic!("missing inner fragment in direct layout: {direct_fragment:#?}"));
  assert!(
    (direct_inner.bounds.y() - 70.0).abs() < 0.5,
    "forced used height should align inner item to bottom (expected y=70, got y={})",
    direct_inner.bounds.y()
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 100.0))
    .expect("layout succeeds");

  let abs_fragment = fragment
    .children
    .iter()
    .find(|fragment| {
      matches!(
        fragment.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == 2
      )
    })
    .unwrap_or_else(|| panic!("missing positioned child fragment: {fragment:#?}"));

  let inner_fragment = abs_fragment
    .children
    .iter()
    .find(|fragment| {
      matches!(
        fragment.content,
        FragmentContent::Block { box_id: Some(box_id) }
          | FragmentContent::Inline { box_id: Some(box_id), .. }
          | FragmentContent::Text { box_id: Some(box_id), .. }
          | FragmentContent::Replaced { box_id: Some(box_id), .. }
          if box_id == 3
      )
    })
    .unwrap_or_else(|| panic!("missing inner flex item fragment: {abs_fragment:#?}"));

  let expected_y = 70.0;
  assert!(
    (inner_fragment.bounds.y() - expected_y).abs() < 0.5,
    "inset-stretched absolute flex containers should relayout against the resolved used height so justify-content:end can align to the bottom (expected y={expected_y}, got y={})",
    inner_fragment.bounds.y()
  );
}
