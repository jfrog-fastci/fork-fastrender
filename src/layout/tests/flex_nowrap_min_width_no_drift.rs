use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::Display;
use crate::style::types::{FlexWrap, JustifyContent};
use crate::style::values::Length;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode};
use crate::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;

fn find_child<'a>(fragment: &'a FragmentNode, box_id: usize) -> &'a FragmentNode {
  fragment
    .children
    .iter()
    .find(|child| match &child.content {
      FragmentContent::Block { box_id: Some(id) }
      | FragmentContent::Inline {
        box_id: Some(id), ..
      }
      | FragmentContent::Text {
        box_id: Some(id), ..
      }
      | FragmentContent::Replaced {
        box_id: Some(id), ..
      } => *id == box_id,
      _ => false,
    })
    .unwrap_or_else(|| panic!("missing child fragment for box_id={box_id}"))
}

#[test]
fn flex_nowrap_min_width_does_not_drift_children_when_width_auto() {
  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::InlineFlex;
  parent_style.flex_wrap = FlexWrap::NoWrap;
  parent_style.justify_content = JustifyContent::FlexEnd;
  parent_style.min_width = Some(Length::px(300.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(50.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;

  let mut first = BoxNode::new_block(
    Arc::new(child_style.clone()),
    FormattingContextType::Block,
    vec![],
  );
  first.id = 1;
  let first_id = first.id;
  let mut second = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  second.id = 2;
  let second_id = second.id;

  let parent = BoxNode::new_block(
    Arc::new(parent_style),
    FormattingContextType::Flex,
    vec![first, second],
  );

  // Constrain the available inline size to 100px. The flex container has `min-width: 300px`, so
  // its used width can legitimately exceed the available space and overflow.
  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite(100.0, 40.0))
    .expect("layout succeeds");

  assert!(
    (fragment.bounds.width() - 300.0).abs() < 1e-3,
    "min-width should be respected (container_w={:.2})",
    fragment.bounds.width()
  );

  let first_frag = find_child(&fragment, first_id);
  let second_frag = find_child(&fragment, second_id);

  assert!(
    (first_frag.bounds.x() - 200.0).abs() < 1e-3,
    "first child should be aligned to the end without drifting past the container (x={:.2})",
    first_frag.bounds.x()
  );
  assert!(
    (second_frag.bounds.x() - 250.0).abs() < 1e-3,
    "second child should follow the first contiguously (x={:.2})",
    second_frag.bounds.x()
  );
  assert!(
    second_frag.bounds.max_x() <= fragment.bounds.width() + 1e-3,
    "children should fit within the min-width container: child_max_x={:.2} container_w={:.2}",
    second_frag.bounds.max_x(),
    fragment.bounds.width()
  );
}
