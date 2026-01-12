use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::float::Float;
use fastrender::style::position::Position;
use fastrender::style::types::InsetValue;
use fastrender::style::types::VerticalAlign;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use std::sync::Arc;

fn find_fragment_by_box_id<'a>(
  fragment: &'a FragmentNode,
  box_id: usize,
) -> Option<&'a FragmentNode> {
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    let matches_id = match &node.content {
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
fn inline_level_float_can_share_line_with_previous_inline_content() {
  // Regression test for inline-level floats encountered after other inline content in a block
  // formatting context.
  //
  // The float should be positioned relative to the current line box, which allows it to share the
  // first line when there is sufficient remaining horizontal space. It must also remain out-of-flow
  // for the purposes of advancing the block cursor: subsequent in-flow blocks should start after
  // the in-flow line boxes, not after the float's full height.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.width = Some(Length::px(500.0));

  let mut left_style = ComputedStyle::default();
  left_style.display = Display::InlineBlock;
  left_style.width = Some(Length::px(200.0));
  left_style.height = Some(Length::px(100.0));
  left_style.vertical_align = VerticalAlign::Top;

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
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

  let mut float =
    BoxNode::new_inline_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
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

  let after_fragment =
    find_fragment_by_box_id(&fragment, 4).expect("after block fragment should exist");
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
fn inline_level_float_relative_positioning_is_visual_only() {
  // Regression test: inline-level floats should still honor `position: relative` offsets without
  // affecting float placement or the flow position of subsequent blocks.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.width = Some(Length::px(500.0));

  let mut left_style = ComputedStyle::default();
  left_style.display = Display::InlineBlock;
  left_style.width = Some(Length::px(200.0));
  left_style.height = Some(Length::px(100.0));
  left_style.vertical_align = VerticalAlign::Top;

  let mut float_style = ComputedStyle::default();
  float_style.display = Display::InlineBlock;
  float_style.float = Float::Right;
  float_style.position = Position::Relative;
  float_style.top = InsetValue::Length(Length::px(20.0));
  float_style.width = Some(Length::px(200.0));
  float_style.height = Some(Length::px(150.0));
  float_style.vertical_align = VerticalAlign::Top;

  let mut after_style = ComputedStyle::default();
  after_style.display = Display::Block;
  after_style.height = Some(Length::px(10.0));

  let mut left =
    BoxNode::new_inline_block(Arc::new(left_style), FormattingContextType::Block, vec![]);
  left.id = 2;

  let mut float =
    BoxNode::new_inline_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
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
    "expected right float to remain aligned to the right edge: bounds={:?}",
    float_fragment.bounds
  );
  assert!(
    (float_fragment.bounds.y() - 20.0).abs() < 0.5,
    "expected float to be visually offset by `top: 20px`: bounds={:?}",
    float_fragment.bounds
  );

  let after_fragment =
    find_fragment_by_box_id(&fragment, 4).expect("after block fragment should exist");
  assert!(
    (after_fragment.bounds.y() - 100.0).abs() < 0.5,
    "expected following block to start after the in-flow line boxes, not after relative float offset: bounds={:?}",
    after_fragment.bounds
  );
}
