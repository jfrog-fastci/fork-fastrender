use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::FormattingContext;
use std::sync::Arc;

fn find_fragment_global_by_box_id<'a>(
  fragment: &'a FragmentNode,
  box_id: usize,
  offset_x: f32,
  offset_y: f32,
) -> Option<(f32, f32, &'a FragmentNode)> {
  let x = offset_x + fragment.bounds.x();
  let y = offset_y + fragment.bounds.y();
  let matches_id = match &fragment.content {
    FragmentContent::Block { box_id: Some(id) }
    | FragmentContent::Inline { box_id: Some(id), .. }
    | FragmentContent::Text { box_id: Some(id), .. }
    | FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
    _ => false,
  };
  if matches_id {
    return Some((x, y, fragment));
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_global_by_box_id(child, box_id, x, y) {
      return Some(found);
    }
  }
  None
}

/// Regression: absolutely positioned descendants should use the padding box of the nearest
/// positioned ancestor as their containing block (CSS 2.1 §10.1).
///
/// In particular, `inset: 0` should align to the padding edge, not extend outward by the padding
/// amounts.
#[test]
fn abspos_inset_zero_uses_padding_box_origin() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.width = Some(Length::px(400.0));

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Block;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(100.0));
  container_style.padding_top = Length::px(30.0);
  container_style.padding_right = Length::px(40.0);
  container_style.padding_bottom = Length::px(50.0);
  container_style.padding_left = Length::px(20.0);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.top = InsetValue::Length(Length::px(0.0));
  abs_style.right = InsetValue::Length(Length::px(0.0));
  abs_style.bottom = InsetValue::Length(Length::px(0.0));
  abs_style.left = InsetValue::Length(Length::px(0.0));

  let mut abs_child =
    BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  abs_child.id = 2;
  let mut container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Block,
    vec![abs_child],
  );
  container.id = 1;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![container],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(400.0, 200.0);
  let fragment = bfc.layout(&root, &constraints).expect("layout should succeed");

  let (container_x, container_y, _) = find_fragment_global_by_box_id(&fragment, 1, 0.0, 0.0)
    .expect("container fragment should exist");
  let (abs_x, abs_y, _) = find_fragment_global_by_box_id(&fragment, 2, 0.0, 0.0)
    .expect("abspos fragment should exist");

  let rel_x = abs_x - container_x;
  let rel_y = abs_y - container_y;

  assert!(
    rel_x.abs() < 0.01,
    "expected abspos inset child to start at the padding edge (x=0), got x={:.2}",
    rel_x
  );
  assert!(
    rel_y.abs() < 0.01,
    "expected abspos inset child to start at the padding edge (y=0), got y={:.2}",
    rel_y
  );
}

