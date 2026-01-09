use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::JustifyContent;
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

fn find_fragment_by_box_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  let matches_id = matches!(
    fragment.content,
    FragmentContent::Block { box_id: Some(box_id) }
      | FragmentContent::Inline { box_id: Some(box_id), .. }
      | FragmentContent::Text { box_id: Some(box_id), .. }
      | FragmentContent::Replaced { box_id: Some(box_id), .. }
      if box_id == id
  );
  if matches_id {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_by_box_id(child, id) {
      return Some(found);
    }
  }
  None
}

#[test]
fn flex_item_flex_container_auto_width_does_not_measure_as_zero() {
  // Regression for flex item measurement where nested block-level flex containers were converted
  // with `width: 100%` (percent) even during intrinsic sizing probes. Percent sizes cannot
  // resolve against max-content/min-content available space, collapsing the item width to ~0 and
  // causing justify-content placement to push it offscreen.

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.justify_content = JustifyContent::SpaceBetween;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(20.0));

  let mut left_style = ComputedStyle::default();
  left_style.display = Display::Block;
  left_style.width = Some(Length::px(50.0));
  left_style.height = Some(Length::px(10.0));
  left_style.flex_shrink = 0.0;
  let mut left = BoxNode::new_block(Arc::new(left_style), FormattingContextType::Block, vec![]);
  left.id = 1;

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.width = Some(Length::px(30.0));
  inner_style.height = Some(Length::px(10.0));
  inner_style.flex_shrink = 0.0;
  let mut inner = BoxNode::new_block(Arc::new(inner_style), FormattingContextType::Block, vec![]);
  inner.id = 3;

  let mut right_style = ComputedStyle::default();
  right_style.display = Display::Flex;
  right_style.height = Some(Length::px(10.0));
  right_style.flex_shrink = 0.0;
  let mut right = BoxNode::new_block(
    Arc::new(right_style),
    FormattingContextType::Flex,
    vec![inner],
  );
  right.id = 2;

  let root = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![left, right],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(200.0, 20.0))
    .expect("layout succeeds");

  let right_fragment =
    find_fragment_by_box_id(&fragment, 2).unwrap_or_else(|| panic!("missing fragment: {fragment:#?}"));

  let eps = 0.1;
  assert!(
    right_fragment.bounds.width() > eps,
    "expected non-zero width; got {:#?}",
    right_fragment.bounds
  );
  assert!(
    right_fragment.bounds.max_x() <= 200.0 + eps,
    "expected right item to fit inside container; got {:#?}",
    right_fragment.bounds
  );
  assert!(
    (right_fragment.bounds.x() - 170.0).abs() <= eps,
    "expected right item to be space-between aligned; got {:#?}",
    right_fragment.bounds
  );
  assert!(
    (right_fragment.bounds.width() - 30.0).abs() <= eps,
    "expected right item to shrink-wrap its contents; got {:#?}",
    right_fragment.bounds
  );
}

