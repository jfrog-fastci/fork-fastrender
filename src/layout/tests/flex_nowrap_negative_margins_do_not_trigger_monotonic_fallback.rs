use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::Display;
use crate::style::types::FlexWrap;
use crate::style::values::Length;
use crate::tree::fragment_tree::FragmentContent;
use crate::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType, FragmentNode};
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

#[test]
fn flex_nowrap_negative_margins_do_not_trigger_monotonic_fallback() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_wrap = FlexWrap::NoWrap;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(20.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(50.0));
  child_style.height = Some(Length::px(10.0));
  child_style.width_keyword = None;
  child_style.height_keyword = None;
  // Avoid flexing so the main size is driven by the authored width/margin.
  child_style.flex_grow = 0.0;
  child_style.flex_shrink = 0.0;

  let first = BoxNode::new_block(
    Arc::new(child_style.clone()),
    FormattingContextType::Block,
    vec![],
  );
  let first_id = first.id;

  let mut second_style = child_style.clone();
  second_style.margin_left = Some(Length::px(-25.0));
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  let second_id = second.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![first, second],
  );
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 20.0))
    .expect("layout succeeds");

  assert_eq!(
    fragment.children.len(),
    2,
    "expected two flex items, got {}",
    fragment.children.len()
  );

  assert_eq!(
    fragment_box_id(&fragment.children[0]),
    Some(first_id),
    "first fragment should correspond to the first box node"
  );
  assert_eq!(
    fragment_box_id(&fragment.children[1]),
    Some(second_id),
    "second fragment should correspond to the second box node"
  );

  let first_frag = &fragment.children[0];
  let second_frag = &fragment.children[1];
  let overlap = first_frag.bounds.max_x() - second_frag.bounds.x();
  assert!(
    (overlap - 25.0).abs() < 0.5,
    "expected the second item to overlap the first by ~25px, got overlap={overlap:.2} (first max_x={:.2}, second x={:.2})",
    first_frag.bounds.max_x(),
    second_frag.bounds.x(),
  );
}
