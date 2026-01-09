use std::sync::Arc;

use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::position::Position;
use fastrender::style::types::{AlignContent, BreakBetween, FlexDirection, FlexWrap, InsetValue};
use fastrender::style::values::Length;
use fastrender::{
  BoxNode, BoxTree, ComputedStyle, FragmentContent, FragmentNode, FragmentTree, LayoutConfig,
  LayoutEngine, Point, Size,
};

fn fragments_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
  let mut out = Vec::new();
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    let node_id = match &node.content {
      FragmentContent::Block { box_id }
      | FragmentContent::Inline { box_id, .. }
      | FragmentContent::Text { box_id, .. }
      | FragmentContent::Replaced { box_id, .. } => *box_id,
      FragmentContent::Line { .. }
      | FragmentContent::RunningAnchor { .. }
      | FragmentContent::FootnoteAnchor { .. } => None,
    };
    if node_id == Some(id) {
      out.push(node);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  out
}

fn first_fragment_offset_in_page(fragment: &FragmentNode, id: usize) -> Option<Point> {
  let root_offset = Point::new(-fragment.bounds.x(), -fragment.bounds.y());
  let mut stack = vec![(fragment, root_offset)];

  while let Some((node, offset)) = stack.pop() {
    let node_offset = offset.translate(node.bounds.origin);
    let node_id = match &node.content {
      FragmentContent::Block { box_id }
      | FragmentContent::Inline { box_id, .. }
      | FragmentContent::Text { box_id, .. }
      | FragmentContent::Replaced { box_id, .. } => *box_id,
      FragmentContent::Line { .. }
      | FragmentContent::RunningAnchor { .. }
      | FragmentContent::FootnoteAnchor { .. } => None,
    };
    if node_id == Some(id) {
      return Some(node_offset);
    }
    for child in node.children.iter() {
      stack.push((child, node_offset));
    }
  }

  None
}

fn pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  std::iter::once(&tree.root)
    .chain(tree.additional_fragments.iter())
    .collect()
}

fn count_running_anchors(fragment: &FragmentNode) -> usize {
  fragment
    .iter_fragments()
    .filter(|node| matches!(node.content, FragmentContent::RunningAnchor { .. }))
    .count()
}

fn flex_container_style() -> ComputedStyle {
  let mut style = ComputedStyle::default();
  style.display = Display::Flex;
  style.flex_direction = FlexDirection::Row;
  style.flex_wrap = FlexWrap::Wrap;
  style.align_content = AlignContent::Start;
  style.width = Some(Length::px(100.0));
  style
}

fn fixed_flex_item_style(width: f32, height: f32) -> ComputedStyle {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(height));
  // Avoid flexing so line breaks are driven by the authored main sizes.
  style.flex_shrink = 0.0;
  style
}

#[test]
fn order_modified_document_order_governs_line_grouping_and_break_propagation() {
  // Create 2 wrapped flex lines (2 items per line) and force a break before the first item in the
  // *second* line after applying CSS `order`. This guards against fragmentation logic that scans
  // DOM order rather than order-modified document order.
  let header = {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.height = Some(Length::px(10.0));
    BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
  };

  let mut a_style = fixed_flex_item_style(50.0, 20.0);
  a_style.order = 0;
  let a = BoxNode::new_block(Arc::new(a_style), FormattingContextType::Block, vec![]);

  let mut b_style = fixed_flex_item_style(50.0, 20.0);
  b_style.order = 2;
  b_style.break_before = BreakBetween::Page;
  let b = BoxNode::new_block(Arc::new(b_style), FormattingContextType::Block, vec![]);

  let mut c_style = fixed_flex_item_style(50.0, 20.0);
  c_style.order = 1;
  let c = BoxNode::new_block(Arc::new(c_style), FormattingContextType::Block, vec![]);

  let mut d_style = fixed_flex_item_style(50.0, 20.0);
  d_style.order = 3;
  let d = BoxNode::new_block(Arc::new(d_style), FormattingContextType::Block, vec![]);

  let flex = BoxNode::new_block(
    Arc::new(flex_container_style()),
    FormattingContextType::Flex,
    vec![a, b, c, d],
  );

  let root_style = Arc::new({
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style
  });
  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![header, flex]);
  let box_tree = BoxTree::new(root);

  let header_id = box_tree.root.children[0].id;
  let flex_items = &box_tree.root.children[1].children;
  let a_id = flex_items[0].id;
  let b_id = flex_items[1].id;
  let c_id = flex_items[2].id;
  let d_id = flex_items[3].id;

  // Everything fits without forced breaks; pagination should happen only because break-before
  // propagates to the flex line boundary.
  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 100.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    tree.additional_fragments.len(),
    1,
    "expected forced flex-line break to create exactly two pages"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  // Sanity: header remains on the first page.
  assert_eq!(fragments_with_id(first_page, header_id).len(), 1);
  assert!(fragments_with_id(second_page, header_id).is_empty());

  // Order-modified first line is A,C; second line is B,D.
  assert_eq!(fragments_with_id(first_page, a_id).len(), 1);
  assert_eq!(fragments_with_id(first_page, c_id).len(), 1);
  assert!(fragments_with_id(first_page, b_id).is_empty());
  assert!(fragments_with_id(first_page, d_id).is_empty());

  assert_eq!(fragments_with_id(second_page, b_id).len(), 1);
  assert_eq!(fragments_with_id(second_page, d_id).len(), 1);
  assert!(fragments_with_id(second_page, a_id).is_empty());
  assert!(fragments_with_id(second_page, c_id).is_empty());

  // Each item should appear exactly once total across pages.
  let pages = pages(&tree);
  for id in [a_id, b_id, c_id, d_id] {
    let count: usize = pages
      .iter()
      .map(|page| fragments_with_id(page, id).len())
      .sum();
    assert_eq!(count, 1, "expected box id {id} to appear exactly once total");
  }
}

#[test]
fn abspos_child_does_not_affect_flex_line_boundaries() {
  // Absolutely positioned descendants are out-of-flow and must not be considered flex items when
  // computing flex line boundaries / atomic ranges for pagination.
  let header = {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.height = Some(Length::px(10.0));
    BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
  };

  let a = BoxNode::new_block(
    Arc::new(fixed_flex_item_style(50.0, 20.0)),
    FormattingContextType::Block,
    vec![],
  );
  let b = BoxNode::new_block(
    Arc::new(fixed_flex_item_style(50.0, 20.0)),
    FormattingContextType::Block,
    vec![],
  );
  let c = BoxNode::new_block(
    Arc::new(fixed_flex_item_style(50.0, 20.0)),
    FormattingContextType::Block,
    vec![],
  );
  let d = BoxNode::new_block(
    Arc::new(fixed_flex_item_style(50.0, 20.0)),
    FormattingContextType::Block,
    vec![],
  );

  let abs_child = {
    // Make the abspos child *wide* enough that if it's accidentally treated as an in-flow flex
    // item, it will create an extra flex line between B and C (and therefore an extra page with the
    // chosen page height below).
    let mut style = fixed_flex_item_style(80.0, 20.0);
    style.position = Position::Absolute;
    style.top = InsetValue::Length(Length::px(0.0));
    style.left = InsetValue::Length(Length::px(0.0));
    BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
  };

  let flex = BoxNode::new_block(
    Arc::new(flex_container_style()),
    FormattingContextType::Flex,
    vec![a, b, abs_child, c, d],
  );

  let root_style = Arc::new({
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style
  });
  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![header, flex]);
  let box_tree = BoxTree::new(root);

  let flex_items = &box_tree.root.children[1].children;
  let a_id = flex_items[0].id;
  let b_id = flex_items[1].id;
  let c_id = flex_items[3].id;
  let d_id = flex_items[4].id;

  // Small page height: should break cleanly between the two flex lines (header=10, line1=20).
  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 35.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    tree.additional_fragments.len(),
    1,
    "expected exactly two pages; out-of-flow abspos children must not create extra flex lines"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  assert_eq!(fragments_with_id(first_page, a_id).len(), 1);
  assert_eq!(fragments_with_id(first_page, b_id).len(), 1);
  assert!(fragments_with_id(first_page, c_id).is_empty());
  assert!(fragments_with_id(first_page, d_id).is_empty());

  assert_eq!(fragments_with_id(second_page, c_id).len(), 1);
  assert_eq!(fragments_with_id(second_page, d_id).len(), 1);
  assert!(fragments_with_id(second_page, a_id).is_empty());
  assert!(fragments_with_id(second_page, b_id).is_empty());

  let c_offset =
    first_fragment_offset_in_page(second_page, c_id).expect("expected C fragment on page 2");
  assert!(
    c_offset.y.abs() < 0.1,
    "second flex line should start at the top of page 2; got y={}",
    c_offset.y
  );
}

#[test]
fn running_anchor_child_does_not_affect_flex_line_boundaries() {
  // Running elements synthesize `FragmentContent::RunningAnchor` children. These anchors must be
  // ignored when computing flex line boundaries and break propagation.
  let header = {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.height = Some(Length::px(10.0));
    BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
  };

  let a = BoxNode::new_block(
    Arc::new(fixed_flex_item_style(50.0, 20.0)),
    FormattingContextType::Block,
    vec![],
  );
  let b = BoxNode::new_block(
    Arc::new(fixed_flex_item_style(50.0, 20.0)),
    FormattingContextType::Block,
    vec![],
  );

  let running = {
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style.running_position = Some("hdr".into());
    // Keep it between B and C in order-modified document order so its anchor is placed at the start
    // of the second line.
    BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![])
  };

  let mut c_style = fixed_flex_item_style(50.0, 20.0);
  c_style.break_before = BreakBetween::Page;
  let c = BoxNode::new_block(Arc::new(c_style), FormattingContextType::Block, vec![]);

  let d = BoxNode::new_block(
    Arc::new(fixed_flex_item_style(50.0, 20.0)),
    FormattingContextType::Block,
    vec![],
  );

  let flex = BoxNode::new_block(
    Arc::new(flex_container_style()),
    FormattingContextType::Flex,
    vec![a, b, running, c, d],
  );

  let root_style = Arc::new({
    let mut style = ComputedStyle::default();
    style.display = Display::Block;
    style
  });
  let root = BoxNode::new_block(root_style, FormattingContextType::Block, vec![header, flex]);
  let box_tree = BoxTree::new(root);

  let flex_children = &box_tree.root.children[1].children;
  let a_id = flex_children[0].id;
  let b_id = flex_children[1].id;
  let running_id = flex_children[2].id;
  let c_id = flex_children[3].id;
  let d_id = flex_children[4].id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 100.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    tree.additional_fragments.len(),
    1,
    "expected forced flex-line break to create exactly two pages"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  assert_eq!(fragments_with_id(first_page, a_id).len(), 1);
  assert_eq!(fragments_with_id(first_page, b_id).len(), 1);
  assert!(fragments_with_id(first_page, c_id).is_empty());
  assert!(fragments_with_id(first_page, d_id).is_empty());

  assert_eq!(fragments_with_id(second_page, c_id).len(), 1);
  assert_eq!(fragments_with_id(second_page, d_id).len(), 1);
  assert!(fragments_with_id(second_page, a_id).is_empty());
  assert!(fragments_with_id(second_page, b_id).is_empty());

  // Running elements are removed from flow, so their box id should not appear in the paginated
  // fragment tree. The synthesized running-anchor fragment should still exist, but must not affect
  // the pagination result above.
  let pages = pages(&tree);
  let running_id_count: usize = pages
    .iter()
    .map(|page| fragments_with_id(page, running_id).len())
    .sum();
  assert_eq!(running_id_count, 0, "running element should not have a box-id fragment");

  let anchor_count: usize = pages.iter().map(|page| count_running_anchors(page)).sum();
  assert!(
    anchor_count > 0,
    "expected at least one running-anchor fragment so the test exercises that path"
  );
}
