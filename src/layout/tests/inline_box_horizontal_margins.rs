use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
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
    return Some((x, y, fragment));
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment_global_by_box_id(child, box_id, x, y) {
      return Some(found);
    }
  }
  None
}

/// Regression: horizontal margins on non-atomic inline boxes (`display: inline`) must contribute to
/// inline advance.
///
/// python.org uses an inline `<a>` button with `margin-right` next to other header content; ignoring
/// inline margins shifts the header alignment.
#[test]
fn inline_box_margin_right_affects_inline_advance() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Inline;
  outer_style.margin_right = Some(Length::px(20.0));

  let mut block_style = ComputedStyle::default();
  block_style.display = Display::InlineBlock;
  block_style.width = Some(Length::px(10.0));
  block_style.height = Some(Length::px(10.0));
  let block_style = Arc::new(block_style);

  let mut inner =
    BoxNode::new_inline_block(block_style.clone(), FormattingContextType::Block, vec![]);
  inner.id = 3;

  let mut outer = BoxNode::new_inline(Arc::new(outer_style), vec![inner]);
  outer.id = 1;

  let mut sibling = BoxNode::new_inline_block(block_style, FormattingContextType::Block, vec![]);
  sibling.id = 2;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![outer, sibling],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 50.0);
  let fragment = bfc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  let (outer_x, _outer_y, outer_frag) = find_fragment_global_by_box_id(&fragment, 1, 0.0, 0.0)
    .expect("outer inline fragment should exist");
  assert!(
    outer_x.abs() < 0.01,
    "expected inline box border box to start at x=0, got x={:.2}",
    outer_x
  );
  assert!(
    (outer_frag.bounds.width() - 10.0).abs() < 0.01,
    "expected inline box border box width=10 (margins excluded), got {:.2}",
    outer_frag.bounds.width()
  );

  let (sibling_x, _sibling_y, _sibling_frag) =
    find_fragment_global_by_box_id(&fragment, 2, 0.0, 0.0).expect("sibling fragment should exist");
  assert!(
    (sibling_x - 30.0).abs() < 0.01,
    "expected sibling to start at x=30 (10px content + 20px margin-right), got x={:.2}",
    sibling_x
  );
}

#[test]
fn inline_box_margin_left_affects_inline_position_and_advance() {
  let mut root_style = ComputedStyle::default();
  root_style.display = Display::Block;

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Inline;
  outer_style.margin_left = Some(Length::px(15.0));
  outer_style.margin_right = Some(Length::px(20.0));

  let mut block_style = ComputedStyle::default();
  block_style.display = Display::InlineBlock;
  block_style.width = Some(Length::px(10.0));
  block_style.height = Some(Length::px(10.0));
  let block_style = Arc::new(block_style);

  let mut inner =
    BoxNode::new_inline_block(block_style.clone(), FormattingContextType::Block, vec![]);
  inner.id = 3;

  let mut outer = BoxNode::new_inline(Arc::new(outer_style), vec![inner]);
  outer.id = 1;

  let mut sibling = BoxNode::new_inline_block(block_style, FormattingContextType::Block, vec![]);
  sibling.id = 2;

  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![outer, sibling],
  );

  let bfc = BlockFormattingContext::new();
  let constraints = LayoutConstraints::definite(200.0, 50.0);
  let fragment = bfc
    .layout(&root, &constraints)
    .expect("layout should succeed");

  let (outer_x, _outer_y, outer_frag) = find_fragment_global_by_box_id(&fragment, 1, 0.0, 0.0)
    .expect("outer inline fragment should exist");
  assert!(
    (outer_x - 15.0).abs() < 0.01,
    "expected inline box border box to start at x=15 (margin-left), got x={:.2}",
    outer_x
  );
  assert!(
    (outer_frag.bounds.width() - 10.0).abs() < 0.01,
    "expected inline box border box width=10 (margins excluded), got {:.2}",
    outer_frag.bounds.width()
  );

  let (sibling_x, _sibling_y, _sibling_frag) =
    find_fragment_global_by_box_id(&fragment, 2, 0.0, 0.0).expect("sibling fragment should exist");
  assert!(
    (sibling_x - 45.0).abs() < 0.01,
    "expected sibling to start at x=45 (15px margin-left + 10px content + 20px margin-right), got x={:.2}",
    sibling_x
  );
}
