use std::sync::Arc;

use crate::layout::fragmentation::FragmentationOptions;
use crate::style::display::{Display, FormattingContextType};
use crate::style::types::{AlignItems, BreakBetween, BreakInside, FlexDirection, FlexWrap};
use crate::style::values::Length;
use crate::{
  BoxNode, BoxTree, ComputedStyle, FragmentContent, FragmentNode, FragmentTree, LayoutConfig,
  LayoutEngine, Point, Size,
};

fn fragments_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
  let mut out = Vec::new();
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if let FragmentContent::Block { box_id: Some(b) } = node.content {
      if b == id {
        out.push(node);
      }
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
    if let FragmentContent::Block { box_id: Some(b) } = node.content {
      if b == id {
        return Some(node_offset);
      }
    }
    for child in node.children.iter() {
      stack.push((child, node_offset));
    }
  }

  None
}

fn paginated_pages<'a>(tree: &'a FragmentTree) -> Vec<&'a FragmentNode> {
  std::iter::once(&tree.root)
    .chain(tree.additional_fragments.iter())
    .collect()
}

fn flex_item_style(width: f32, height: f32) -> Arc<ComputedStyle> {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.width = Some(Length::px(width));
  style.height = Some(Length::px(height));
  style.width_keyword = None;
  style.height_keyword = None;
  // Avoid flexing so line breaks are driven by the authored main sizes.
  style.flex_shrink = 0.0;
  Arc::new(style)
}

#[test]
fn flex_column_item_break_inside_avoid_page_moves_to_next_page() {
  const EPSILON: f32 = 0.1;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  let container_style = Arc::new(container_style);

  // Page height 50:
  // - Item A: 30px tall, fits on page 1.
  // - Item B: 30px tall and `break-inside: avoid-page`. It would start at y=30 and be sliced by the
  //   50px boundary, but it fits within a single page so it should move (unbroken) to page 2.
  // - Item C: follows B and should move along with B (not overlap B on page 2).
  let item_a = BoxNode::new_block(
    flex_item_style(100.0, 30.0),
    FormattingContextType::Block,
    vec![],
  );

  let mut item_b_style = (*flex_item_style(100.0, 30.0)).clone();
  item_b_style.break_inside = BreakInside::AvoidPage;
  let item_b = BoxNode::new_block(Arc::new(item_b_style), FormattingContextType::Block, vec![]);

  let item_c = BoxNode::new_block(
    flex_item_style(100.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );

  let flex = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![item_a, item_b, item_c],
  );
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![flex],
  );
  let box_tree = BoxTree::new(root);

  let flex_box = &box_tree.root.children[0];
  let item_b_id = flex_box.children[1].id;
  let item_c_id = flex_box.children[2].id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 50.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert!(
    tree.additional_fragments.len() >= 1,
    "expected flex container to paginate"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  assert!(
    fragments_with_id(first_page, item_b_id).is_empty(),
    "expected avoid-page flex item B to be moved wholly to page 2"
  );
  assert!(
    fragments_with_id(first_page, item_c_id).is_empty(),
    "expected following item C to move along with item B"
  );

  let b_frags = fragments_with_id(second_page, item_b_id);
  assert_eq!(b_frags.len(), 1, "expected flex item B to appear once");
  assert!(
    (b_frags[0].bounds.height() - 30.0).abs() < EPSILON,
    "expected flex item B to retain its full height when moved; got {}",
    b_frags[0].bounds.height()
  );

  let c_frags = fragments_with_id(second_page, item_c_id);
  assert_eq!(c_frags.len(), 1, "expected flex item C to appear once on page 2");
  let c_offset =
    first_fragment_offset_in_page(second_page, item_c_id).expect("expected item C on page 2");
  assert!(
    (c_offset.y - 30.0).abs() < EPSILON,
    "expected item C to be positioned after the moved item B (y≈30), got y={}",
    c_offset.y
  );

  let pages = paginated_pages(&tree);
  for id in [item_b_id, item_c_id] {
    let count: usize = pages
      .iter()
      .map(|page| fragments_with_id(page, id).len())
      .sum();
    assert_eq!(count, 1, "expected box id {id} to appear exactly once total");
  }
}

#[test]
fn flex_row_wrap_item_break_inside_avoid_page_does_not_force_siblings() {
  const EPSILON: f32 = 0.1;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_items = AlignItems::Start;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  let container_style = Arc::new(container_style);

  // Page height 25.
  //
  // Line 1: A(100x10)
  // Line 2: B(50x20, break-inside: avoid-page) + C(50x40)
  //
  // Line2 is taller than the page because of C, so it must fragment. `break-inside: avoid-page` on
  // B should keep B intact by moving it to the next page *without* forcing sibling C (or the first
  // line) onto the next page.
  let item_a = BoxNode::new_block(
    flex_item_style(100.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );

  let mut item_b_style = (*flex_item_style(50.0, 20.0)).clone();
  item_b_style.break_inside = BreakInside::AvoidPage;
  let item_b = BoxNode::new_block(Arc::new(item_b_style), FormattingContextType::Block, vec![]);

  let item_c = BoxNode::new_block(
    flex_item_style(50.0, 40.0),
    FormattingContextType::Block,
    vec![],
  );

  let flex = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![item_a, item_b, item_c],
  );
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![flex],
  );
  let box_tree = BoxTree::new(root);

  let flex_box = &box_tree.root.children[0];
  let item_b_id = flex_box.children[1].id;
  let item_c_id = flex_box.children[2].id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 25.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert!(
    tree.additional_fragments.len() >= 1,
    "expected flex container to paginate"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  assert!(
    fragments_with_id(first_page, item_b_id).is_empty(),
    "expected avoid-page flex item B to move to page 2 (not be sliced on page 1)"
  );
  // Sibling item C is taller than the page; it should still have a fragment on page 1.
  let c_first = fragments_with_id(first_page, item_c_id);
  assert_eq!(
    c_first.len(),
    1,
    "expected tall sibling flex item C to remain on page 1"
  );
  assert!(
    (c_first[0].bounds.height() - 15.0).abs() < EPSILON,
    "expected C to be clipped to the remaining space on page 1 (height≈15), got {}",
    c_first[0].bounds.height()
  );

  let b_second = fragments_with_id(second_page, item_b_id);
  assert_eq!(b_second.len(), 1, "expected B to appear exactly once on page 2");
  assert!(
    (b_second[0].bounds.height() - 20.0).abs() < EPSILON,
    "expected B to retain its full height on page 2; got {}",
    b_second[0].bounds.height()
  );
  let b_offset =
    first_fragment_offset_in_page(second_page, item_b_id).expect("expected B on page 2");
  assert!(
    b_offset.y.abs() < EPSILON,
    "expected B to start at the top of page 2; got y={}",
    b_offset.y
  );

  let c_second = fragments_with_id(second_page, item_c_id);
  assert_eq!(
    c_second.len(),
    1,
    "expected C to have a continuation fragment on page 2"
  );
  assert!(
    (c_second[0].bounds.height() - 25.0).abs() < EPSILON,
    "expected C's continuation to fill the page (height≈25), got {}",
    c_second[0].bounds.height()
  );

  let pages = paginated_pages(&tree);
  // B should appear exactly once total (moved), while C should appear twice (fragmented).
  let b_total: usize = pages
    .iter()
    .map(|page| fragments_with_id(page, item_b_id).len())
    .sum();
  assert_eq!(b_total, 1, "expected B to appear exactly once total");
  let c_total: usize = pages
    .iter()
    .map(|page| fragments_with_id(page, item_c_id).len())
    .sum();
  assert_eq!(c_total, 2, "expected C to be fragmented across two pages");
}

#[test]
fn flex_item_break_inside_avoid_page_can_fragment_when_taller_than_page() {
  const EPSILON: f32 = 0.1;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Column;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  let container_style = Arc::new(container_style);

  // A single 80px-tall flex item with `break-inside: avoid-page` should still fragment across 50px
  // pages because it cannot fit in a single fragmentainer.
  let mut item_style = (*flex_item_style(100.0, 80.0)).clone();
  item_style.break_inside = BreakInside::AvoidPage;
  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);

  let flex = BoxNode::new_block(container_style, FormattingContextType::Flex, vec![item]);
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![flex],
  );
  let box_tree = BoxTree::new(root);

  let item_id = box_tree.root.children[0].children[0].id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 50.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert!(
    tree.additional_fragments.len() >= 1,
    "expected tall avoid-page flex item to paginate"
  );
  let pages = paginated_pages(&tree);
  let item_fragments: Vec<&FragmentNode> = pages
    .iter()
    .flat_map(|page| fragments_with_id(page, item_id))
    .collect();
  assert!(
    item_fragments.len() >= 2,
    "expected tall avoid-page flex item to be fragmented across multiple pages"
  );

  // At least one fragment should be clipped (height < original 80px).
  assert!(
    item_fragments
      .iter()
      .any(|frag| frag.bounds.height() < 80.0 - EPSILON),
    "expected at least one fragment slice to be clipped"
  );
}

#[test]
fn flex_pagination_does_not_slice_within_flex_line_when_line_can_fit() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_items = AlignItems::Start;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  let container_style = Arc::new(container_style);

  // Two lines of items, 2 per line:
  // Line1: A(h=10), B(h=10) -> line height 10
  // Line2: C(h=4),  D(h=10) -> line height 10, but C ends early at y=14
  // Page height 15 creates a tempting "break after C" boundary at y=14.
  let item_a = BoxNode::new_block(
    flex_item_style(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  let item_b = BoxNode::new_block(
    flex_item_style(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  let item_c = BoxNode::new_block(
    flex_item_style(50.0, 4.0),
    FormattingContextType::Block,
    vec![],
  );
  let item_d = BoxNode::new_block(
    flex_item_style(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );

  let flex = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![item_a, item_b, item_c, item_d],
  );
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![flex],
  );
  let box_tree = BoxTree::new(root);

  let flex_box = &box_tree.root.children[0];
  let c_id = flex_box.children[2].id;
  let d_id = flex_box.children[3].id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 15.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert!(
    tree.additional_fragments.len() >= 1,
    "flex container should span at least two pages"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  assert!(
    fragments_with_id(first_page, c_id).is_empty(),
    "expected the second flex line to be pushed to the next page instead of slicing within the line"
  );
  assert!(
    fragments_with_id(first_page, d_id).is_empty(),
    "expected the second flex line to be pushed to the next page instead of slicing within the line"
  );
  assert_eq!(
    fragments_with_id(second_page, c_id).len(),
    1,
    "expected item C to appear exactly once on page 2"
  );
  assert_eq!(
    fragments_with_id(second_page, d_id).len(),
    1,
    "expected item D to appear exactly once on page 2"
  );

  let pages = paginated_pages(&tree);
  for id in [c_id, d_id] {
    let count: usize = pages
      .iter()
      .map(|page| fragments_with_id(page, id).len())
      .sum();
    assert_eq!(
      count, 1,
      "expected box id {id} to appear exactly once total across all pages"
    );
  }
}

#[test]
fn flex_column_fragmentation_does_not_slice_within_flex_line_when_line_can_fit() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  container_style.align_items = AlignItems::Start;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  let container_style = Arc::new(container_style);

  let item_a = BoxNode::new_block(
    flex_item_style(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  let item_b = BoxNode::new_block(
    flex_item_style(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  let item_c = BoxNode::new_block(
    flex_item_style(50.0, 4.0),
    FormattingContextType::Block,
    vec![],
  );
  let item_d = BoxNode::new_block(
    flex_item_style(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );

  let flex = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![item_a, item_b, item_c, item_d],
  );
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![flex],
  );
  let box_tree = BoxTree::new(root);

  let flex_box = &box_tree.root.children[0];
  let c_id = flex_box.children[2].id;
  let d_id = flex_box.children[3].id;

  // Fragment into two 15px-tall columns; the 10px-tall second flex line should move entirely into
  // the second column (rather than splitting at the 4px-tall item C boundary inside the line).
  let fragmentation = FragmentationOptions::new(15.0).with_columns(2, 0.0);
  let engine = LayoutEngine::new(
    LayoutConfig::for_viewport(Size::new(200.0, 200.0)).with_fragmentation(fragmentation),
  );
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    tree.additional_fragments.len(),
    1,
    "expected two columns (root + 1 additional fragment)"
  );
  let first_column = &tree.root;
  let second_column = &tree.additional_fragments[0];

  assert!(
    fragments_with_id(first_column, c_id).is_empty(),
    "expected the second flex line to be pushed into the second column instead of slicing within the line"
  );
  assert!(
    fragments_with_id(first_column, d_id).is_empty(),
    "expected the second flex line to be pushed into the second column instead of slicing within the line"
  );
  assert_eq!(
    fragments_with_id(second_column, c_id).len(),
    1,
    "expected item C to appear exactly once in column 2"
  );
  assert_eq!(
    fragments_with_id(second_column, d_id).len(),
    1,
    "expected item D to appear exactly once in column 2"
  );

  let pages = paginated_pages(&tree);
  for id in [c_id, d_id] {
    let count: usize = pages
      .iter()
      .map(|page| fragments_with_id(page, id).len())
      .sum();
    assert_eq!(
      count, 1,
      "expected box id {id} to appear exactly once total across all columns"
    );
  }
}

#[test]
fn flex_pagination_break_before_propagates_to_line_boundary() {
  const EPSILON: f32 = 0.1;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.flex_wrap = FlexWrap::Wrap;
  // `align-items: flex-end` ensures the second item in the second line (D) is positioned below the
  // line's start edge, so a naive "break before D" would land inside the line.
  container_style.align_items = AlignItems::End;
  container_style.width = Some(Length::px(100.0));
  container_style.width_keyword = None;
  let container_style = Arc::new(container_style);

  let item_a = BoxNode::new_block(
    flex_item_style(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );
  let item_b = BoxNode::new_block(
    flex_item_style(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );

  let item_c = BoxNode::new_block(
    flex_item_style(50.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );

  let mut item_d_style = (*flex_item_style(50.0, 4.0)).clone();
  item_d_style.break_before = BreakBetween::Page;
  let item_d = BoxNode::new_block(Arc::new(item_d_style), FormattingContextType::Block, vec![]);

  let flex = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![item_a, item_b, item_c, item_d],
  );
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![flex],
  );
  let box_tree = BoxTree::new(root);

  let flex_box = &box_tree.root.children[0];
  let c_id = flex_box.children[2].id;
  let d_id = flex_box.children[3].id;

  // The page is large enough to fit everything; pagination should only happen when the forced
  // break is honoured at the flex line boundary (start of line2).
  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 100.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    tree.additional_fragments.len(),
    1,
    "expected forced break to create exactly two pages"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  assert!(
    fragments_with_id(first_page, c_id).is_empty(),
    "expected line2 to be moved entirely to page 2 (C should not appear on page 1)"
  );
  assert!(
    fragments_with_id(first_page, d_id).is_empty(),
    "expected line2 to be moved entirely to page 2 (D should not appear on page 1)"
  );
  assert_eq!(
    fragments_with_id(second_page, c_id).len(),
    1,
    "expected item C to appear exactly once on page 2"
  );
  assert_eq!(
    fragments_with_id(second_page, d_id).len(),
    1,
    "expected item D to appear exactly once on page 2"
  );

  let offset = first_fragment_offset_in_page(second_page, c_id).expect("expected item C on page 2");
  assert!(
    offset.y.abs() < EPSILON,
    "expected the second flex line to start at the top of page 2; got y={}",
    offset.y
  );
}

#[test]
fn flex_pagination_forced_break_inside_item_does_not_force_siblings() {
  const EPSILON: f32 = 0.1;

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::Row;
  container_style.width = Some(Length::px(200.0));
  container_style.width_keyword = None;
  let container_style = Arc::new(container_style);

  let mut item_a_style = ComputedStyle::default();
  item_a_style.display = Display::Block;
  item_a_style.width = Some(Length::px(100.0));
  item_a_style.width_keyword = None;
  item_a_style.flex_shrink = 0.0;
  let item_a_style = Arc::new(item_a_style);

  let mut a1_style = ComputedStyle::default();
  a1_style.display = Display::Block;
  a1_style.height = Some(Length::px(10.0));
  a1_style.height_keyword = None;
  a1_style.break_after = BreakBetween::Page;
  let a1_style = Arc::new(a1_style);

  let mut a2_style = ComputedStyle::default();
  a2_style.display = Display::Block;
  a2_style.height = Some(Length::px(10.0));
  a2_style.height_keyword = None;
  let a2_style = Arc::new(a2_style);

  let a1 = BoxNode::new_block(a1_style, FormattingContextType::Block, vec![]);
  let a2 = BoxNode::new_block(a2_style, FormattingContextType::Block, vec![]);
  let item_a = BoxNode::new_block(item_a_style, FormattingContextType::Block, vec![a1, a2]);

  let item_b = BoxNode::new_block(
    flex_item_style(100.0, 20.0),
    FormattingContextType::Block,
    vec![],
  );

  let flex = BoxNode::new_block(
    container_style,
    FormattingContextType::Flex,
    vec![item_a, item_b],
  );
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![flex],
  );
  let box_tree = BoxTree::new(root);

  let flex_box = &box_tree.root.children[0];
  let item_a_box = &flex_box.children[0];
  let a1_id = item_a_box.children[0].id;
  let a2_id = item_a_box.children[1].id;
  let b_id = flex_box.children[1].id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 100.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    tree.additional_fragments.len(),
    1,
    "expected forced break to create exactly two pages"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  assert_eq!(
    fragments_with_id(first_page, a1_id).len(),
    1,
    "expected first block in flex item A to live on the first page"
  );
  assert!(
    fragments_with_id(first_page, a2_id).is_empty(),
    "expected forced break inside flex item A to move its continuation to the next page"
  );

  let b_fragments = fragments_with_id(first_page, b_id);
  assert_eq!(
    b_fragments.len(),
    1,
    "expected sibling flex item B to remain on the first page"
  );
  assert!(
    (b_fragments[0].bounds.height() - 20.0).abs() < EPSILON,
    "expected sibling flex item B to keep its full height on page 1; got {}",
    b_fragments[0].bounds.height()
  );

  assert_eq!(
    fragments_with_id(second_page, a2_id).len(),
    1,
    "expected continuation content on the second page"
  );
  assert!(
    fragments_with_id(second_page, b_id).is_empty(),
    "expected sibling flex item B to not be forced onto the second page"
  );

  let pages = paginated_pages(&tree);
  for id in [a1_id, a2_id, b_id] {
    let count: usize = pages
      .iter()
      .map(|page| fragments_with_id(page, id).len())
      .sum();
    assert_eq!(
      count, 1,
      "expected box id {id} to appear exactly once total"
    );
  }
}

#[test]
fn flex_pagination_does_not_split_row_gap_across_pages() {
  const EPSILON: f32 = 0.1;

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Row;
  flex_style.flex_wrap = FlexWrap::Wrap;
  flex_style.align_items = AlignItems::Start;
  flex_style.width = Some(Length::px(100.0));
  flex_style.width_keyword = None;
  flex_style.grid_row_gap = Length::px(10.0);
  flex_style.grid_row_gap_is_normal = false;
  flex_style.grid_column_gap = Length::px(0.0);
  flex_style.grid_column_gap_is_normal = false;
  let flex_style = Arc::new(flex_style);

  let item_a = BoxNode::new_block(
    flex_item_style(100.0, 30.0),
    FormattingContextType::Block,
    vec![],
  );
  let item_b = BoxNode::new_block(
    flex_item_style(100.0, 10.0),
    FormattingContextType::Block,
    vec![],
  );

  let flex = BoxNode::new_block(
    flex_style,
    FormattingContextType::Flex,
    vec![item_a, item_b],
  );
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![flex],
  );
  let box_tree = BoxTree::new(root);

  let item_b_id = box_tree.root.children[0].children[1].id;

  // Page height ends 5px into the row-gap (30px line + 5px into the 10px gutter). The break should
  // snap to the end edge of the first line so the full 10px gap is preserved on the next page.
  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 35.0), 0.0));
  let tree = engine.layout_tree(&box_tree).expect("layout");

  assert!(
    tree.additional_fragments.len() >= 1,
    "flex container should span at least two pages"
  );
  let first_page = &tree.root;
  let second_page = &tree.additional_fragments[0];

  assert!(
    fragments_with_id(first_page, item_b_id).is_empty(),
    "expected the second line to appear only on the second page"
  );
  assert_eq!(
    fragments_with_id(second_page, item_b_id).len(),
    1,
    "expected the second line to appear exactly once on the second page"
  );

  let offset = first_fragment_offset_in_page(second_page, item_b_id)
    .expect("expected to find second line fragment on page 2");
  assert!(
    (offset.y - 10.0).abs() < EPSILON,
    "expected the full 10px row-gap to be preserved on page 2, got y={}",
    offset.y
  );

  let pages = paginated_pages(&tree);
  let count: usize = pages
    .iter()
    .map(|page| fragments_with_id(page, item_b_id).len())
    .sum();
  assert_eq!(
    count, 1,
    "expected box id {item_b_id} to appear exactly once total across all pages"
  );
}
