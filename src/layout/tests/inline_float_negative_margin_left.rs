use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::float::Float;
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
fn inline_float_negative_margin_left_affects_float_fit() {
  // Bootstrap button groups rely on `margin-left: -1px` on adjacent floats so borders overlap
  // without forcing a wrap. Inline layout handles floats encountered in inline content; it must
  // preserve negative horizontal margins when computing the float's margin box.

  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;
  root_style.width = Some(Length::px(199.0));

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::InlineBlock;
  first_style.float = Float::Left;
  first_style.width = Some(Length::px(100.0));
  first_style.height = Some(Length::px(10.0));

  let mut second_style = first_style.clone();
  second_style.margin_left = Some(Length::px(-1.0));

  let mut first =
    BoxNode::new_inline_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  first.id = 2;

  let mut second =
    BoxNode::new_inline_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  second.id = 3;

  let mut root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![first, second],
  );
  root.id = 1;

  let constraints = LayoutConstraints::definite(199.0, 100.0);
  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  let first_fragment =
    find_fragment_by_box_id(&fragment, 2).expect("first float fragment should exist");
  let second_fragment =
    find_fragment_by_box_id(&fragment, 3).expect("second float fragment should exist");

  assert!(
    first_fragment.bounds.y().abs() < 0.5,
    "expected first float on the first line: bounds={:?}",
    first_fragment.bounds
  );
  assert!(
    second_fragment.bounds.y().abs() < 0.5,
    "expected second float to stay on the first line: bounds={:?}",
    second_fragment.bounds
  );
  assert!(
    (second_fragment.bounds.x() - 99.0).abs() < 0.5,
    "expected negative margin-left to shift the second float left by 1px: bounds={:?}",
    second_fragment.bounds
  );
}
