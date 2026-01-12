use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::flex::FlexFormattingContext;
use crate::style::display::Display;
use crate::style::types::FlexDirection;
use crate::style::types::FlexWrap;
use crate::style::types::Overflow;
use crate::style::values::Length;
use crate::tree::fragment_tree::FragmentContent;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContext;
use crate::FormattingContextType;
use std::sync::Arc;

fn fragment_box_id(fragment: &crate::FragmentNode) -> Option<usize> {
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
fn flex_wrap_order_does_not_trigger_manual_main_axis_placement() {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(20.0));
  container_style.width_keyword = None;
  container_style.height_keyword = None;
  // The manual placement fallback is only enabled for wrap containers with non-scrollable overflow.
  container_style.overflow_x = Overflow::Visible;
  container_style.overflow_y = Overflow::Visible;

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Block;
  a_style.width = Some(Length::px(50.0));
  a_style.height = Some(Length::px(10.0));
  a_style.width_keyword = None;
  a_style.height_keyword = None;
  a_style.flex_shrink = 0.0;
  a_style.order = 1;
  let mut child_a = BoxNode::new_block(Arc::new(a_style), FormattingContextType::Block, vec![]);
  child_a.id = 1;

  let mut b_style = ComputedStyle::default();
  b_style.display = Display::Block;
  b_style.width = Some(Length::px(50.0));
  b_style.height = Some(Length::px(10.0));
  b_style.width_keyword = None;
  b_style.height_keyword = None;
  b_style.flex_shrink = 0.0;
  b_style.order = 0;
  let mut child_b = BoxNode::new_block(Arc::new(b_style), FormattingContextType::Block, vec![]);
  child_b.id = 2;

  // DOM order: [A, B] but flex order: [B, A].
  let a_id = child_a.id;
  let b_id = child_b.id;

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );

  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(200.0, 20.0))
    .expect("layout succeeds");

  let mut a_x = None;
  let mut b_x = None;
  let mut min_x = f32::INFINITY;
  let mut debug_children = Vec::new();
  for child in fragment.children.iter() {
    let id = fragment_box_id(child);
    debug_children.push((
      id,
      child.bounds.x(),
      child.bounds.y(),
      child.bounds.width(),
      child.bounds.height(),
    ));
    match id {
      Some(id) if id == a_id => a_x = Some(child.bounds.x()),
      Some(id) if id == b_id => b_x = Some(child.bounds.x()),
      _ => {}
    }
    if id == Some(a_id) || id == Some(b_id) {
      min_x = min_x.min(child.bounds.x());
    }
  }

  let a_x = a_x.unwrap_or_else(|| panic!("A fragment present: {:?}", debug_children));
  let b_x = b_x.unwrap_or_else(|| panic!("B fragment present: {:?}", debug_children));

  assert!(
    (b_x - 0.0).abs() < 1e-3,
    "order=0 item should be first at x=0, got x={b_x:.2} (children: {debug_children:?})"
  );
  assert!(
    (a_x - 50.0).abs() < 1e-3,
    "order=1 item should follow at x=50, got x={a_x:.2} (children: {debug_children:?})"
  );
  assert!(
    (min_x - 0.0).abs() < 1e-3,
    "min x among flex items should be 0 (no gap at start), got min_x={min_x:.2} (children: {debug_children:?})"
  );
}
