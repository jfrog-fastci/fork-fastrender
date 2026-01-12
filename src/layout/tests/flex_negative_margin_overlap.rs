use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::types::{FlexDirection, JustifyContent};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::{BoxNode, ComputedStyle, FormattingContext};
use std::sync::Arc;

#[test]
fn flex_negative_margin_overlap_preserved_in_auto_height_container() {
  // Regression: Negative main-axis margins in flex layout can legitimately make adjacent items
  // overlap. The flex container must preserve the overlap (instead of reflowing items contiguously)
  // and `justify-content` must not introduce leading offsets when the container's block size is
  // auto (content-sized).
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::Center;
  container_style.width = Some(Length::px(200.0));

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.width = Some(Length::px(200.0));
  first_style.height = Some(Length::px(100.0));
  first_style.flex_shrink = 0.0;
  let mut first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  first.id = 1;

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.width = Some(Length::px(200.0));
  second_style.height = Some(Length::px(100.0));
  second_style.margin_top = Some(Length::px(-20.0));
  second_style.flex_shrink = 0.0;
  let mut second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  second.id = 2;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![first, second],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite_width(200.0))
    .expect("layout succeeds");

  let mut first_y = None;
  let mut second_y = None;
  for child in fragment.children.iter() {
    let id = match &child.content {
      FragmentContent::Block { box_id }
      | FragmentContent::Inline { box_id, .. }
      | FragmentContent::Replaced { box_id, .. }
      | FragmentContent::Text { box_id, .. } => *box_id,
      FragmentContent::Line { .. }
      | FragmentContent::RunningAnchor { .. }
      | FragmentContent::FootnoteAnchor { .. } => None,
    };
    match id {
      Some(1) => first_y = Some(child.bounds.y()),
      Some(2) => second_y = Some(child.bounds.y()),
      _ => {}
    }
  }

  let first_y = first_y.expect("first child fragment");
  let second_y = second_y.expect("second child fragment");

  assert!(
    (first_y - 0.0).abs() < 0.01,
    "first child should start at y=0, got {first_y}"
  );
  assert!(
    (second_y - 80.0).abs() < 0.01,
    "second child should overlap by 20px (y=80), got {second_y}"
  );
  assert!(
    (fragment.bounds.height() - 180.0).abs() < 0.01,
    "container height should shrink to content (180px), got {}",
    fragment.bounds.height()
  );
}
