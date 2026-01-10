use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::float::Float;
use fastrender::style::types::VerticalAlign;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::anonymous::AnonymousBoxCreator;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::box_tree::ReplacedType;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use std::sync::Arc;
use fastrender::Size;

fn find_fragment_by_box_id<'a>(fragment: &'a FragmentNode, box_id: usize) -> Option<&'a FragmentNode> {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    let matches_id = match &node.content {
      FragmentContent::Block { box_id: Some(id) }
      | FragmentContent::Inline { box_id: Some(id), .. }
      | FragmentContent::Text { box_id: Some(id), .. }
      | FragmentContent::Replaced { box_id: Some(id), .. } => *id == box_id,
      _ => false,
    };
    if matches_id {
      return Some(node);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  None
}

#[test]
fn block_level_float_can_share_line_with_previous_inline_content() {
  // Floats generate block-level boxes (CSS 2.1 §9.5.1), but when a float is encountered after
  // other inline content in the source, it can still float up next to that content on the current
  // line box (as long as there is horizontal room). If block layout flushes buffered inline
  // content before placing the float, the float is forced below the already-laid-out line boxes.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.width = Some(Length::px(500.0));

  let mut left_style = ComputedStyle::default();
  left_style.display = Display::InlineBlock;
  left_style.width = Some(Length::px(200.0));
  left_style.height = Some(Length::px(100.0));
  left_style.vertical_align = VerticalAlign::Top;

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::Block;
  float_style.float = Float::Right;
  float_style.width = Some(Length::px(200.0));
  float_style.height = Some(Length::px(150.0));
  float_style.vertical_align = VerticalAlign::Top;

  let mut after_style = ComputedStyle::default();
  after_style.display = Display::Block;
  after_style.height = Some(Length::px(10.0));

  let mut left =
    BoxNode::new_inline_block(Arc::new(left_style), FormattingContextType::Block, vec![]);
  left.id = 2;

  let mut float = BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
  float.id = 3;

  let mut after = BoxNode::new_block(Arc::new(after_style), FormattingContextType::Block, vec![]);
  after.id = 4;

  let mut root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![left, float, after],
  );
  root.id = 1;

  let constraints = LayoutConstraints::definite(500.0, 500.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc.layout(&root, &constraints).expect("block layout");

  let float_fragment = find_fragment_by_box_id(&fragment, 3).expect("float fragment should exist");
  assert!(
    (float_fragment.bounds.x() - 300.0).abs() < 0.5,
    "expected right float to be positioned at the right edge: bounds={:?}",
    float_fragment.bounds
  );
  assert!(
    float_fragment.bounds.y().abs() < 0.5,
    "expected float to share the first line: bounds={:?}",
    float_fragment.bounds
  );

  let after_fragment = find_fragment_by_box_id(&fragment, 4).expect("after block fragment should exist");
  assert!(
    (after_fragment.bounds.y() - 100.0).abs() < 0.5,
    "expected following block to start after the in-flow line boxes, not below the float: bounds={:?}",
    after_fragment.bounds
  );
  assert!(
    after_fragment.bounds.y() < float_fragment.bounds.max_y() - 0.5,
    "expected following block to start above the float's bottom edge (floats are out-of-flow): float={:?} after={:?}",
    float_fragment.bounds,
    after_fragment.bounds
  );
}

#[test]
fn anonymous_fixup_does_not_split_inline_runs_around_floats() {
  // Anonymous block fixup should not treat floats as in-flow block-level children. Otherwise, an
  // inline run like `<label>…</label><input style=float:right>` is split into an anonymous block
  // followed by a sibling float, preventing the float from sharing the first line (e.g. the
  // Arch Linux pkgsearch widget).

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.width = Some(Length::px(300.0));

  let mut label_style = ComputedStyle::default();
  label_style.display = Display::InlineBlock;
  label_style.width = Some(Length::px(100.0));
  label_style.height = Some(Length::px(20.0));
  label_style.vertical_align = VerticalAlign::Top;

  let mut float_style = ComputedStyle::default();
  // Floats are blockified during box generation (CSS Display §2.1 / CSS 2.1 §9.7), so simulate a
  // post-blockification display type here.
  float_style.display = Display::FlowRoot;
  float_style.float = Float::Right;
  float_style.width = Some(Length::px(100.0));
  float_style.height = Some(Length::px(20.0));

  let mut label =
    BoxNode::new_inline_block(Arc::new(label_style), FormattingContextType::Block, vec![]);
  label.id = 2;

  let mut float = BoxNode::new_replaced(
    Arc::new(float_style),
    ReplacedType::Canvas,
    Some(Size::new(100.0, 20.0)),
    Some(100.0 / 20.0),
  );
  float.id = 3;

  let mut root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![label, float],
  );
  root.id = 1;

  let fixed = AnonymousBoxCreator::fixup_tree(root).expect("anonymous fixup");
  assert!(
    fixed.children.iter().any(|c| c.id == 2),
    "expected inline child to remain a direct child after anonymous fixup"
  );
  assert!(
    fixed.children.iter().any(|c| c.id == 3),
    "expected floated child to remain a direct child after anonymous fixup"
  );

  let fc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(300.0, 200.0);
  let fragment = fc.layout(&fixed, &constraints).expect("block layout");

  let float_fragment = find_fragment_by_box_id(&fragment, 3).expect("float fragment should exist");
  assert!(
    (float_fragment.bounds.x() - 200.0).abs() < 0.5,
    "expected right float to be positioned at the right edge: bounds={:?}",
    float_fragment.bounds
  );
  assert!(
    float_fragment.bounds.y().abs() < 0.5,
    "expected float to share the first line: bounds={:?}",
    float_fragment.bounds
  );
}
