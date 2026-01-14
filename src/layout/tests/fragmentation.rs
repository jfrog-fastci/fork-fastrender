use std::sync::Arc;

use crate::layout::fragmentation::{
  fragment_tree, resolve_fragmentation_boundaries_with_context, FragmentationContext,
  FragmentationOptions,
};
use crate::style::display::{Display, FormattingContextType};
use crate::style::position::Position;
use crate::style::types::{
  BreakBetween, BreakInside, FlexDirection, GridTrack, InsetValue, IntrinsicSizeKeyword,
  WritingMode,
};
use crate::style::values::Length;
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::{GridFragmentationInfo, GridItemFragmentationData};
use crate::{
  BoxTree, ComputedStyle, FastRender, FragmentContent, FragmentNode, FragmentTree, LayoutConfig,
  LayoutEngine, Point, Rect, Size,
};

fn line(y: f32, height: f32) -> FragmentNode {
  FragmentNode::new_line(Rect::from_xywh(0.0, y, 80.0, height), height * 0.8, vec![])
}

fn count_lines(node: &FragmentNode) -> usize {
  node
    .iter_fragments()
    .filter(|fragment| matches!(fragment.content, FragmentContent::Line { .. }))
    .count()
}

#[test]
fn paragraph_breaks_on_line_boundaries() {
  let lines: Vec<_> = (0..6).map(|i| line(i as f32 * 12.0, 12.0)).collect();
  let para_height = lines.last().map(|l| l.bounds.max_y()).unwrap_or(0.0);
  let paragraph =
    FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 120.0, para_height), lines.clone());
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 120.0, para_height),
    vec![paragraph],
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(24.0)).unwrap();

  assert_eq!(fragments.len(), 3);
  let per_fragment: Vec<_> = fragments.iter().map(count_lines).collect();
  assert_eq!(per_fragment, vec![2, 2, 2]);
  assert_eq!(per_fragment.iter().sum::<usize>(), lines.len());

  for fragment in &fragments {
    for line in fragment
      .iter_fragments()
      .filter(|f| matches!(f.content, FragmentContent::Line { .. }))
    {
      assert!(
        (line.bounds.height() - 12.0).abs() < 0.01,
        "lines must not be clipped"
      );
    }
  }
}

fn collect_lines<'a>(fragment: &'a FragmentNode) -> Vec<&'a FragmentNode> {
  let mut lines = Vec::new();
  let mut stack = vec![fragment];
  while let Some(node) = stack.pop() {
    if matches!(node.content, FragmentContent::Line { .. }) {
      lines.push(node);
    }
    for child in node.children.iter() {
      stack.push(child);
    }
  }
  lines
}

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

#[test]
fn pagination_respects_gap_and_forced_break() {
  let mut breaker_style = ComputedStyle::default();
  breaker_style.break_after = BreakBetween::Always;
  let breaker = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
    vec![],
    Arc::new(breaker_style),
  );
  let follower = FragmentNode::new_block(Rect::from_xywh(0.0, 30.0, 50.0, 20.0), vec![]);
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 100.0, 140.0),
    vec![breaker, follower],
  );

  let options = FragmentationOptions::new(80.0).with_gap(20.0);
  let fragments = fragment_tree(&root, &options).unwrap();

  assert!(
    fragments.len() >= 3,
    "forced break + overflow should yield multiple fragments"
  );
  assert!((fragments[1].bounds.y() - 100.0).abs() < 0.01);
  assert!((fragments[2].bounds.y() - 200.0).abs() < 0.01);
  assert!(fragments[0]
    .children
    .iter()
    .all(|child| child.bounds.y() >= 0.0 && child.bounds.max_y() <= 80.0));
  assert!(fragments[1]
    .children
    .iter()
    .any(|child| matches!(child.content, FragmentContent::Block { .. })));
}

#[test]
fn pagination_without_candidates_uses_fragmentainer_size() {
  // Single tall block without explicit break opportunities should still fragment by height.
  let block = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 40.0, 150.0), vec![]);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 150.0), vec![block]);

  let fragments = fragment_tree(&root, &FragmentationOptions::new(60.0)).unwrap();

  assert_eq!(fragments.len(), 3);
  assert!((fragments[1].bounds.y() - 60.0).abs() < 0.1);
  assert!((fragments[2].bounds.y() - 120.0).abs() < 0.1);
}

#[test]
fn break_opportunity_just_after_fragmentainer_limit_does_not_slice_previous_box() {
  // Regression: if a between-sibling break opportunity starts just after the fragmentainer limit
  // (within floating point epsilon), fragmentation should not select a boundary *before* the
  // opportunity. Otherwise the preceding box gets sliced, producing a near-zero continuation
  // fragment on the next page/column.
  let first = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 100.005), 1, vec![]);
  let second = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 150.0, 40.0, 10.0), 2, vec![]);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 160.0), vec![first, second]);

  let fragments = fragment_tree(&root, &FragmentationOptions::new(100.0)).unwrap();
  assert_eq!(fragments.len(), 2);

  assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
  assert!(fragments_with_id(&fragments[1], 1).is_empty());
  assert_eq!(fragments_with_id(&fragments[1], 2).len(), 1);
}

#[test]
fn forced_break_before_first_child_does_not_create_leading_empty_fragment() {
  let mut child_style = ComputedStyle::default();
  child_style.break_before = BreakBetween::Page;
  let child_style = Arc::new(child_style);

  // Simulate a first child that starts after a padding-like offset.
  let mut child =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 20.0, 40.0, 10.0), 1, vec![]);
  child.style = Some(child_style);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 40.0), vec![child]);

  // The forced break is at the start of the content flow; it should not create a fragmentainer
  // slice containing only the leading offset.
  let fragments = fragment_tree(&root, &FragmentationOptions::new(200.0)).unwrap();

  assert_eq!(fragments.len(), 1);
  assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
}

#[test]
fn forced_break_after_last_child_propagates_to_parent_end() {
  let mut breaker_style = ComputedStyle::default();
  breaker_style.break_after = BreakBetween::Column;
  let breaker_style = Arc::new(breaker_style);

  // The last child ends early relative to its parent (simulating trailing padding/align-content
  // space). A forced break-after should propagate to the parent's end edge so the parent isn't
  // split into a padding-only continuation fragment.
  let mut breaker =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 10.0), 1, vec![]);
  breaker.style = Some(breaker_style);
  let parent =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 100.0, 40.0), 10, vec![breaker]);

  let follower =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 40.0, 40.0, 10.0), 20, vec![]);
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 100.0, 50.0),
    vec![parent, follower],
  );

  let fragments =
    fragment_tree(&root, &FragmentationOptions::new(50.0).with_columns(2, 0.0)).unwrap();
  assert_eq!(
    fragments.len(),
    2,
    "expected content to fragment after the forced break"
  );

  assert_eq!(fragments_with_id(&fragments[0], 10).len(), 1);
  assert_eq!(
    fragments_with_id(&fragments[1], 10).len(),
    0,
    "parent should not be split into a trailing continuation fragment"
  );

  assert_eq!(fragments_with_id(&fragments[0], 20).len(), 0);
  let follower_frags = fragments_with_id(&fragments[1], 20);
  assert_eq!(follower_frags.len(), 1);
  assert!(
    follower_frags[0].bounds.y().abs() < 0.1,
    "expected following content to start at the top of the next column"
  );
}

#[test]
fn flex_item_forced_break_does_not_force_sibling_breaks() {
  let mut breaker_style = ComputedStyle::default();
  breaker_style.break_after = BreakBetween::Page;
  let breaker_style = Arc::new(breaker_style);

  let mut breaker =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 20.0), 1, vec![]);
  breaker.style = Some(breaker_style);
  let follower = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 20.0, 40.0, 20.0), 2, vec![]);

  let item_a = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
    10,
    vec![breaker, follower],
  );
  let item_b = FragmentNode::new_block_with_id(Rect::from_xywh(50.0, 0.0, 40.0, 40.0), 20, vec![]);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Row;
  let flex = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 40.0),
    vec![item_a, item_b],
    Arc::new(flex_style),
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 40.0), vec![flex]);

  let fragments = fragment_tree(&root, &FragmentationOptions::new(50.0)).unwrap();
  assert_eq!(
    fragments.len(),
    2,
    "expected content to fragment after blank insertion"
  );

  assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
  assert_eq!(
    fragments_with_id(&fragments[0], 2).len(),
    0,
    "post-break content should not be on the first page"
  );
  assert_eq!(
    fragments_with_id(&fragments[0], 20).len(),
    1,
    "sibling flex item should remain on the first page"
  );

  assert_eq!(
    fragments_with_id(&fragments[1], 2).len(),
    1,
    "post-break content should appear on the next page"
  );
  assert_eq!(
    fragments_with_id(&fragments[1], 20).len(),
    0,
    "sibling flex item must not be duplicated or split onto the next page"
  );
}

#[test]
fn flex_item_forced_column_break_does_not_force_sibling_breaks() {
  let mut breaker_style = ComputedStyle::default();
  breaker_style.break_after = BreakBetween::Column;
  let breaker_style = Arc::new(breaker_style);

  let mut breaker =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 20.0), 1, vec![]);
  breaker.style = Some(breaker_style);
  let follower = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 20.0, 40.0, 20.0), 2, vec![]);

  let item_a = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
    10,
    vec![breaker, follower],
  );
  let item_b = FragmentNode::new_block_with_id(Rect::from_xywh(50.0, 0.0, 40.0, 40.0), 20, vec![]);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Row;
  let flex = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 40.0),
    vec![item_a, item_b],
    Arc::new(flex_style),
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 40.0), vec![flex]);

  let fragments =
    fragment_tree(&root, &FragmentationOptions::new(50.0).with_columns(2, 0.0)).unwrap();
  assert_eq!(
    fragments.len(),
    2,
    "expected content to fragment after blank insertion"
  );

  assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
  assert_eq!(
    fragments_with_id(&fragments[0], 2).len(),
    0,
    "post-break content should not be in the first column"
  );
  assert_eq!(
    fragments_with_id(&fragments[0], 20).len(),
    1,
    "sibling flex item should remain in the first column"
  );

  assert_eq!(
    fragments_with_id(&fragments[1], 2).len(),
    1,
    "post-break content should appear in the next column"
  );
  assert_eq!(
    fragments_with_id(&fragments[1], 20).len(),
    0,
    "sibling flex item must not be duplicated or split into the next column"
  );
}

#[test]
fn grid_item_forced_column_break_does_not_force_sibling_breaks() {
  let mut breaker_style = ComputedStyle::default();
  breaker_style.break_after = BreakBetween::Column;
  let breaker_style = Arc::new(breaker_style);

  let mut breaker =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 20.0), 1, vec![]);
  breaker.style = Some(breaker_style);
  let follower = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 20.0, 40.0, 20.0), 2, vec![]);

  let item_a = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
    10,
    vec![breaker, follower],
  );
  let item_b = FragmentNode::new_block_with_id(Rect::from_xywh(50.0, 0.0, 40.0, 40.0), 20, vec![]);

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  let mut grid = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 40.0),
    vec![item_a, item_b],
    Arc::new(grid_style),
  );
  grid.grid_fragmentation = Some(Arc::new(GridFragmentationInfo {
    items: vec![
      GridItemFragmentationData {
        box_id: 10,
        row_start: 1,
        row_end: 2,
        column_start: 1,
        column_end: 2,
      },
      GridItemFragmentationData {
        box_id: 20,
        row_start: 1,
        row_end: 2,
        column_start: 2,
        column_end: 3,
      },
    ],
  }));
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, 40.0), vec![grid]);

  let options = FragmentationOptions::new(50.0).with_columns(2, 0.0);
  let fragments = fragment_tree(&root, &options).unwrap();
  assert_eq!(
    fragments.len(),
    2,
    "expected content to fragment after blank insertion"
  );

  assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
  assert!(
    fragments_with_id(&fragments[0], 2).is_empty(),
    "post-break content should not be in the first column"
  );
  assert_eq!(
    fragments_with_id(&fragments[0], 20).len(),
    1,
    "sibling grid item should remain in the first column"
  );

  let follower_frags = fragments_with_id(&fragments[1], 2);
  assert_eq!(
    follower_frags.len(),
    1,
    "post-break content should appear in the next column"
  );
  assert!(
    follower_frags[0].bounds.y().abs() < 0.1,
    "expected the continuation content to start at the top of the next column"
  );
  assert!(
    fragments_with_id(&fragments[1], 20).is_empty(),
    "sibling grid item must not be duplicated or split onto the next column"
  );
}

#[test]
fn abspos_parallel_flow_forced_page_break_creates_additional_fragmentainer() {
  let fragmentainer_size = 100.0;

  let a = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 50.0), 1, vec![]);
  let b = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 50.0, 40.0, 50.0), 2, vec![]);

  let mut abs1_style = ComputedStyle::default();
  abs1_style.break_after = BreakBetween::Page;
  let mut abs1 =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 20.0), 3, vec![]);
  abs1.style = Some(Arc::new(abs1_style));

  let abs2 =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 20.0, 40.0, 20.0), 4, vec![]);

  let mut abspos_style = ComputedStyle::default();
  abspos_style.position = Position::Absolute;
  let mut abspos = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
    10,
    vec![abs1, abs2],
  );
  abspos.style = Some(Arc::new(abspos_style));

  // Root is only tall enough for the in-flow content. The abspos continuation should still force
  // an additional fragmentainer slice.
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 100.0, fragmentainer_size),
    vec![a, b, abspos],
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(fragmentainer_size)).unwrap();
  assert_eq!(fragments.len(), 2);

  assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
  assert_eq!(fragments_with_id(&fragments[0], 2).len(), 1);
  assert!(fragments_with_id(&fragments[1], 1).is_empty());
  assert!(fragments_with_id(&fragments[1], 2).is_empty());

  assert!(
    fragments_with_id(&fragments[0], 4).is_empty(),
    "post-break abspos content must not appear in the first fragmentainer"
  );
  assert_eq!(
    fragments_with_id(&fragments[1], 4).len(),
    1,
    "post-break abspos content should appear in the next fragmentainer"
  );
}

#[test]
fn abspos_break_before_after_are_ignored_in_parent_flow() {
  let fragmentainer_size = 100.0;

  let a = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 50.0), 1, vec![]);
  let b = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 50.0, 40.0, 50.0), 2, vec![]);

  let abs1 =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 20.0), 3, vec![]);
  let abs2 =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 20.0, 40.0, 20.0), 4, vec![]);

  let mut abspos_style = ComputedStyle::default();
  abspos_style.position = Position::Absolute;
  abspos_style.break_before = BreakBetween::Page;
  abspos_style.break_after = BreakBetween::Page;
  let mut abspos = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
    10,
    vec![abs1, abs2],
  );
  abspos.style = Some(Arc::new(abspos_style));

  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 100.0, fragmentainer_size),
    vec![a, b, abspos],
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(fragmentainer_size)).unwrap();
  assert_eq!(
    fragments.len(),
    1,
    "break-before/after on abspos should not force additional fragments when everything fits"
  );
}

#[test]
fn abspos_parallel_flow_forced_column_break_does_not_force_in_flow_siblings() {
  let column_height = 100.0;

  let a = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 50.0), 1, vec![]);
  let b = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 50.0, 40.0, 50.0), 2, vec![]);

  let mut abs1_style = ComputedStyle::default();
  abs1_style.break_after = BreakBetween::Column;
  let mut abs1 =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 20.0), 3, vec![]);
  abs1.style = Some(Arc::new(abs1_style));

  let abs2 =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 20.0, 40.0, 20.0), 4, vec![]);

  let mut abspos_style = ComputedStyle::default();
  abspos_style.position = Position::Absolute;
  let mut abspos = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 40.0, 40.0),
    10,
    vec![abs1, abs2],
  );
  abspos.style = Some(Arc::new(abspos_style));

  // Root is only tall enough for the in-flow content. The abspos continuation should still create
  // an additional column fragment.
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 100.0, column_height),
    vec![a, b, abspos],
  );

  let options = FragmentationOptions::new(column_height).with_columns(2, 0.0);
  let fragments = fragment_tree(&root, &options).unwrap();
  assert_eq!(
    fragments.len(),
    2,
    "forced break inside abspos parallel flow should create a continuation column"
  );

  assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
  assert_eq!(fragments_with_id(&fragments[0], 2).len(), 1);
  assert!(fragments_with_id(&fragments[1], 1).is_empty());
  assert!(fragments_with_id(&fragments[1], 2).is_empty());

  assert!(
    fragments_with_id(&fragments[0], 4).is_empty(),
    "post-break abspos content must not appear in the first column"
  );
  assert_eq!(
    fragments_with_id(&fragments[1], 4).len(),
    1,
    "post-break abspos content should appear in the continuation column"
  );
}

#[test]
fn vertical_writing_fragment_tree_columns_use_inline_axis() {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.writing_mode = WritingMode::VerticalLr;
  let style = Arc::new(style);

  let child = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 150.0, 40.0),
    vec![],
    style.clone(),
  );
  let root =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 150.0, 40.0), vec![child], style);

  let fragments = fragment_tree(
    &root,
    &FragmentationOptions::new(60.0).with_columns(2, 10.0),
  )
  .unwrap();

  assert_eq!(fragments.len(), 3);
  assert_eq!(fragments[0].bounds.origin, Point::ZERO);
  assert!((fragments[1].bounds.x()).abs() < 0.01);
  assert!((fragments[1].bounds.y() - (40.0 + 10.0)).abs() < 0.01);
  assert!((fragments[2].bounds.x() - 60.0).abs() < 0.01);
  assert!((fragments[2].bounds.y()).abs() < 0.01);

  for (idx, fragment) in fragments.iter().enumerate() {
    assert_eq!(
      fragment.children.len(),
      1,
      "fragment {idx} should preserve the lone child"
    );
    let fragment_child = &fragment.children[0];
    let slice_start = (idx as f32) * 60.0;
    let expected_block_size = (root.bounds.width() - slice_start).min(60.0);
    assert!(
      (fragment_child.bounds.width() - expected_block_size).abs() < 0.01,
      "fragment {idx} child width should match the clipped block slice"
    );
  }
}

#[test]
fn vertical_lr_fragmentation_clips_on_x_and_sets_slice_info() {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.writing_mode = WritingMode::VerticalLr;
  let style = Arc::new(style);

  let mut child1 =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 60.0), 1, vec![]);
  child1.style = Some(style.clone());
  let mut child2 =
    FragmentNode::new_block_with_id(Rect::from_xywh(40.0, 0.0, 40.0, 60.0), 2, vec![]);
  child2.style = Some(style.clone());

  let root = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 80.0, 60.0),
    vec![child1, child2],
    style,
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(50.0)).unwrap();

  assert_eq!(fragments.len(), 2);

  let first_child2 = fragments_with_id(&fragments[0], 2);
  let second_child2 = fragments_with_id(&fragments[1], 2);

  assert_eq!(first_child2.len(), 1);
  assert_eq!(second_child2.len(), 1);

  let first_slice = first_child2[0];
  let second_slice = second_child2[0];

  assert!((first_slice.bounds.width() - 10.0).abs() < 0.01);
  assert!((second_slice.bounds.width() - 30.0).abs() < 0.01);

  assert!(first_slice.slice_info.is_first);
  assert!(!first_slice.slice_info.is_last);
  assert!(first_slice.slice_info.slice_offset.abs() < 0.01);
  assert!((first_slice.slice_info.original_block_size - 40.0).abs() < 0.01);

  assert!(!second_slice.slice_info.is_first);
  assert!(second_slice.slice_info.is_last);
  assert!((second_slice.slice_info.slice_offset - 10.0).abs() < 0.01);
  assert!((second_slice.slice_info.original_block_size - 40.0).abs() < 0.01);
}

#[test]
fn vertical_rl_fragmentation_block_negative_slice_info() {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.writing_mode = WritingMode::VerticalRl;
  let style = Arc::new(style);

  let mut child1 =
    FragmentNode::new_block_with_id(Rect::from_xywh(40.0, 0.0, 40.0, 60.0), 1, vec![]);
  child1.style = Some(style.clone());
  let mut child2 =
    FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 60.0), 2, vec![]);
  child2.style = Some(style.clone());

  let root = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 80.0, 60.0),
    vec![child1, child2],
    style,
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(50.0)).unwrap();

  assert_eq!(fragments.len(), 2);

  let first_child2 = fragments_with_id(&fragments[0], 2);
  let second_child2 = fragments_with_id(&fragments[1], 2);

  assert_eq!(first_child2.len(), 1);
  assert_eq!(second_child2.len(), 1);

  let first_slice = first_child2[0];
  let second_slice = second_child2[0];

  assert!((first_slice.bounds.width() - 10.0).abs() < 0.01);
  assert!((second_slice.bounds.width() - 30.0).abs() < 0.01);

  assert!(first_slice.slice_info.is_first);
  assert!(!first_slice.slice_info.is_last);
  assert!(first_slice.slice_info.slice_offset.abs() < 0.01);
  assert!((first_slice.slice_info.original_block_size - 40.0).abs() < 0.01);

  assert!(!second_slice.slice_info.is_first);
  assert!(second_slice.slice_info.is_last);
  assert!((second_slice.slice_info.slice_offset - 10.0).abs() < 0.01);
  assert!((second_slice.slice_info.original_block_size - 40.0).abs() < 0.01);
}

#[test]
fn vertical_writing_fragment_stacking_uses_x_for_gap() {
  let mut style = ComputedStyle::default();
  style.display = Display::Block;
  style.writing_mode = WritingMode::VerticalLr;
  let style = Arc::new(style);

  let child =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 80.0, 20.0), vec![], style.clone());
  let root =
    FragmentNode::new_block_styled(Rect::from_xywh(0.0, 0.0, 80.0, 20.0), vec![child], style);

  let fragments = fragment_tree(&root, &FragmentationOptions::new(50.0).with_gap(20.0)).unwrap();

  assert_eq!(fragments.len(), 2);
  assert!(fragments[0].bounds.x().abs() < 0.01);
  assert!(fragments[0].bounds.y().abs() < 0.01);
  assert!((fragments[1].bounds.x() - 70.0).abs() < 0.01);
  assert!(fragments[1].bounds.y().abs() < 0.01);
}

#[test]
fn widows_and_orphans_keep_paragraph_together() {
  let mut para_style = ComputedStyle::default();
  para_style.break_inside = BreakInside::Auto;
  para_style.widows = 3;
  para_style.orphans = 3;
  let lines = vec![
    line(0.0, 15.0),
    line(18.0, 15.0),
    line(36.0, 15.0),
    line(54.0, 15.0),
  ];
  let paragraph = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 120.0, 80.0),
    lines,
    Arc::new(para_style),
  );
  let footer = FragmentNode::new_block(Rect::from_xywh(0.0, 90.0, 100.0, 10.0), vec![]);
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 120.0, 120.0),
    vec![paragraph, footer],
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(50.0)).unwrap();

  assert!(
    fragments.len() >= 2,
    "content should span multiple fragmentainers when it overflows"
  );

  let total_lines: usize = fragments.iter().map(count_lines).sum();
  assert_eq!(total_lines, 4, "all line fragments should be retained");
}

#[test]
fn line_fragments_remain_atomic_when_boundary_slices_through() {
  let atomic_line = line(10.0, 15.0);
  let original_height = atomic_line.bounds.height();
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 120.0, 25.0), vec![atomic_line]);

  let fragments = fragment_tree(&root, &FragmentationOptions::new(15.0)).unwrap();

  assert_eq!(fragments.len(), 2);
  let first_fragment_lines = count_lines(&fragments[0]);
  let second_fragment_lines: Vec<&FragmentNode> = fragments[1]
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Line { .. }))
    .collect();
  assert_eq!(first_fragment_lines, 0);
  assert_eq!(second_fragment_lines.len(), 1);
  assert!(
    (second_fragment_lines[0].bounds.height() - original_height).abs() < 0.01,
    "line fragments should keep their full height even when clipped mid-line"
  );
  assert!(
    fragments.iter().map(count_lines).sum::<usize>() == 1,
    "line should only appear once across fragments"
  );
}

#[test]
fn widows_and_orphans_enforced_across_multiple_breaks() {
  let mut style = ComputedStyle::default();
  style.widows = 2;
  style.orphans = 2;

  let lines: Vec<_> = (0..10).map(|i| line(i as f32 * 10.0, 10.0)).collect();
  let para_height = lines.last().map(|l| l.bounds.max_y()).unwrap_or(0.0);
  let paragraph = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 120.0, para_height),
    lines,
    Arc::new(style),
  );
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 120.0, para_height),
    vec![paragraph],
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(30.0)).unwrap();
  let per_fragment: Vec<_> = fragments
    .iter()
    .map(count_lines)
    .filter(|c| *c > 0)
    .collect();

  assert_eq!(per_fragment.iter().sum::<usize>(), 10);
  assert_eq!(per_fragment, vec![3, 3, 2, 2]);
  assert!(per_fragment.iter().all(|count| *count >= 2));
}

#[test]
fn break_inside_avoid_prefers_unbroken_but_splits_when_needed() {
  let mut avoid_style = ComputedStyle::default();
  avoid_style.break_inside = BreakInside::Avoid;
  let avoid_style = Arc::new(avoid_style);

  // Fits entirely within the first fragmentainer.
  let fitting_lines: Vec<_> = (0..3).map(|i| line(i as f32 * 12.0, 12.0)).collect();
  let fitting_block = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 36.0),
    fitting_lines,
    avoid_style.clone(),
  );
  let trailing = FragmentNode::new_block(Rect::from_xywh(0.0, 40.0, 100.0, 20.0), vec![]);
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 120.0, 60.0),
    vec![fitting_block, trailing],
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(50.0)).unwrap();
  let per_fragment: Vec<_> = fragments.iter().map(count_lines).collect();
  assert_eq!(per_fragment.iter().sum::<usize>(), 3);
  assert_eq!(per_fragment[0], 3);

  // Taller than a fragmentainer: must break even with avoid.
  let tall_lines: Vec<_> = (0..6).map(|i| line(i as f32 * 12.0, 12.0)).collect();
  let tall_block = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, 72.0),
    tall_lines.clone(),
    avoid_style,
  );
  let tall_root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 120.0, 72.0), vec![tall_block]);

  let tall_fragments = fragment_tree(&tall_root, &FragmentationOptions::new(40.0)).unwrap();
  let tall_counts: Vec<_> = tall_fragments.iter().map(count_lines).collect();
  assert!(tall_fragments.len() > 1);
  assert_eq!(tall_counts.iter().sum::<usize>(), tall_lines.len());
  assert!(tall_counts.iter().all(|count| *count > 0));
}

#[test]
fn forced_break_inside_avoid_still_splits_pages() {
  let mut avoid_style = ComputedStyle::default();
  avoid_style.break_inside = BreakInside::Avoid;
  let avoid_style = Arc::new(avoid_style);

  let mut breaker_style = ComputedStyle::default();
  breaker_style.break_after = BreakBetween::Always;
  let breaker_style = Arc::new(breaker_style);

  let mut first = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 50.0, 30.0), 1, vec![]);
  first.style = Some(breaker_style);
  let second = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 30.0, 50.0, 60.0), 2, vec![]);

  let mut outer = FragmentNode::new_block_with_id(
    Rect::from_xywh(0.0, 0.0, 50.0, 90.0),
    3,
    vec![first, second],
  );
  outer.style = Some(avoid_style);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 90.0), vec![outer]);

  // The outer avoid-inside block fits within a single fragmentainer; without special handling the
  // atomic range for `break-inside: avoid` would suppress the forced break.
  let fragments = fragment_tree(&root, &FragmentationOptions::new(100.0)).unwrap();
  assert_eq!(
    fragments.len(),
    2,
    "forced break inside avoid range should still create a new fragment"
  );

  assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
  assert!(fragments_with_id(&fragments[0], 2).is_empty());
  assert!(fragments_with_id(&fragments[1], 1).is_empty());
  assert_eq!(fragments_with_id(&fragments[1], 2).len(), 1);
}

#[test]
fn forced_break_overrides_natural_flow() {
  let mut breaker_style = ComputedStyle::default();
  breaker_style.break_after = BreakBetween::Always;
  let breaker = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 50.0, 30.0),
    vec![],
    Arc::new(breaker_style),
  );
  let follower = FragmentNode::new_block(Rect::from_xywh(0.0, 30.0, 50.0, 30.0), vec![]);
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 50.0, 60.0),
    vec![breaker, follower],
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(200.0)).unwrap();

  assert_eq!(fragments.len(), 2);
  assert_eq!(fragments[0].children.len(), 1);
  assert_eq!(fragments[1].children.len(), 1);
}

#[test]
fn column_break_hints_follow_column_context() {
  let mut first_style = ComputedStyle::default();
  first_style.break_after = BreakBetween::Column;
  let first = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
    vec![],
    Arc::new(first_style),
  );
  let second = FragmentNode::new_block(Rect::from_xywh(0.0, 20.0, 50.0, 20.0), vec![]);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 40.0), vec![first, second]);

  let options = FragmentationOptions::new(80.0).with_columns(2, 0.0);
  let fragments = fragment_tree(&root, &options).unwrap();

  assert_eq!(
    fragments.len(),
    2,
    "forced column break should split fragments even when content fits"
  );

  let first_children: Vec<_> = fragments[0]
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Block { .. }))
    .collect();
  let second_children: Vec<_> = fragments[1]
    .children
    .iter()
    .filter(|child| matches!(child.content, FragmentContent::Block { .. }))
    .collect();

  assert_eq!(
    first_children.len(),
    1,
    "first fragment should only include the pre-break block"
  );
  assert_eq!(
    second_children.len(),
    1,
    "second fragment should only include the post-break block"
  );
  assert_eq!(
    first_children[0]
      .style
      .as_ref()
      .map(|s| s.break_after)
      .unwrap_or(BreakBetween::Auto),
    BreakBetween::Column
  );
  assert_eq!(
    second_children[0]
      .style
      .as_ref()
      .map(|s| s.break_after)
      .unwrap_or(BreakBetween::Auto),
    BreakBetween::Auto
  );
}

#[test]
fn break_inside_avoid_keeps_block_together() {
  let mut avoid_style = ComputedStyle::default();
  avoid_style.break_inside = BreakInside::Avoid;
  let tall_block = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 80.0, 140.0),
    vec![],
    Arc::new(avoid_style),
  );
  let trailing = FragmentNode::new_block(Rect::from_xywh(0.0, 150.0, 50.0, 20.0), vec![]);
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 120.0, 200.0),
    vec![tall_block, trailing],
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(80.0)).unwrap();

  assert_eq!(
    fragments.len(),
    3,
    "avoid is a soft constraint and tall content may split"
  );
  let trailing_height: f32 = fragments
    .last()
    .unwrap()
    .children
    .iter()
    .map(|c| c.bounds.height())
    .sum();
  assert!(
    (trailing_height - 20.0).abs() < 0.1,
    "trailing block should appear in the final fragment"
  );
}

#[test]
fn avoid_page_blocks_aren_t_split_across_pages() {
  let mut avoid_style = ComputedStyle::default();
  avoid_style.break_inside = BreakInside::AvoidPage;
  let avoid_style = Arc::new(avoid_style);

  let leading = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 40.0, 6.0), 1, vec![]);
  let mut avoid = FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 6.0, 40.0, 8.0), 2, vec![]);
  avoid.style = Some(avoid_style);
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 20.0), vec![leading, avoid]);

  let fragments = fragment_tree(&root, &FragmentationOptions::new(10.0)).unwrap();

  assert!(
    fragments.len() >= 2,
    "overflowing content should fragment across pages"
  );

  assert!(
    fragments_with_id(&fragments[0], 2).is_empty(),
    "avoid-page content should be pushed out of the fragment that would slice it"
  );
  let avoid_fragments: Vec<_> = fragments
    .iter()
    .flat_map(|fragment| fragments_with_id(fragment, 2))
    .collect();
  assert_eq!(
    avoid_fragments.len(),
    1,
    "avoid-page content should stay intact across pagination"
  );
  assert!(
    avoid_fragments[0].fragment_index > 0,
    "avoid-page block should be moved wholly into a later fragmentainer"
  );
  assert!(
    (avoid_fragments[0].bounds.height() - 8.0).abs() < 0.1,
    "avoid-page block should retain its full height when fragmented"
  );
}

#[test]
fn positioned_children_follow_fragmentainers() {
  let normal = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 60.0, 40.0), vec![]);
  let abs_child = FragmentNode::new_block(Rect::from_xywh(0.0, 120.0, 40.0, 20.0), vec![]);
  let root = FragmentNode::new_block(
    Rect::from_xywh(0.0, 0.0, 100.0, 180.0),
    vec![normal, abs_child],
  );

  let fragments = fragment_tree(&root, &FragmentationOptions::new(80.0)).unwrap();

  assert_eq!(fragments.len(), 3);
  let positioned_home = fragments.iter().position(|fragment| {
    fragment
      .children
      .iter()
      .any(|c| (c.bounds.height() - 20.0).abs() < 0.1)
  });
  assert!(positioned_home.expect("positioned fragment placed") > 0);
}

#[test]
fn layout_engine_pagination_splits_pages() {
  let mut style = ComputedStyle::default();
  style.height = Some(Length::px(150.0));
  let root_box = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
  let box_tree = BoxTree::new(root_box);

  let config = LayoutConfig::for_pagination(Size::new(200.0, 60.0), 10.0);
  let engine = LayoutEngine::new(config);
  let fragments = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(fragments.additional_fragments.len(), 2);
  assert!(fragments.root.fragment_count >= 3);
  assert!((fragments.additional_fragments[0].bounds.y() - 70.0).abs() < 0.1);
  assert!((fragments.additional_fragments[1].bounds.y() - 140.0).abs() < 0.1);
}

#[test]
fn grid_continuation_reduces_available_block_size_for_items() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(100.0));
  // Use three rows so the target item starts part-way into a continuation fragment (y=120px).
  // With a 100px fragmentainer height, that leaves 80px remaining.
  grid_style.height = Some(Length::px(220.0));
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  grid_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(60.0)),
    GridTrack::Length(Length::px(60.0)),
    GridTrack::Length(Length::px(100.0)),
  ];

  let item1 = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![],
  );

  let item2 = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![],
  );

  let mut item3_style = ComputedStyle::default();
  item3_style.height_keyword = Some(IntrinsicSizeKeyword::FillAvailable);
  let inner_style = Arc::new({
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(10.0));
    style
  });
  let inner = BoxNode::new_block(inner_style, FormattingContextType::Block, vec![]);
  let item3 = BoxNode::new_block(
    Arc::new(item3_style),
    FormattingContextType::Block,
    vec![inner],
  );

  let root_box = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item1, item2, item3],
  );
  let box_tree = BoxTree::new(root_box);
  let item3_id = box_tree.root.children.get(2).expect("third grid item").id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(100.0, 100.0), 0.0));
  let fragments = engine.layout_tree(&box_tree).expect("layout");

  // Sanity-check the unfragmented flow positions so the continuation heuristic has stable inputs.
  let unfragmented_engine = LayoutEngine::new(LayoutConfig::for_viewport(Size::new(100.0, 100.0)));
  let unfragmented = unfragmented_engine.layout_tree(&box_tree).expect("layout");
  let unfragmented_item3 = fragments_with_id(&unfragmented.root, item3_id);
  assert_eq!(unfragmented_item3.len(), 1);
  assert!(
    (unfragmented_item3[0].bounds.y() - 120.0).abs() < 0.1,
    "expected grid item 3 to start in the third row (got y={})",
    unfragmented_item3[0].bounds.y()
  );
  assert!(
    (unfragmented_item3[0].bounds.height() - 100.0).abs() < 0.1,
    "expected grid item 3 to fill its 100px row track before pagination (got height={})",
    unfragmented_item3[0].bounds.height()
  );

  let mut item3_fragment = None;
  for page in &fragments.additional_fragments {
    let matches = fragments_with_id(page, item3_id);
    if let Some(found) = matches.first() {
      item3_fragment = Some(*found);
      break;
    }
  }
  let item3_fragment = item3_fragment.expect("grid item 3 should appear in a continuation page");
  assert!(
    (item3_fragment.bounds.height() - 80.0).abs() < 0.1,
    "expected continuation grid item to shrink to the remaining 80px of the fragmentainer (got {})",
    item3_fragment.bounds.height()
  );
  assert!(
    (item3_fragment.bounds.width() - 100.0).abs() < 0.1,
    "expected continuation grid item width to remain unchanged (got {})",
    item3_fragment.bounds.width()
  );
}

#[test]
fn vertical_writing_mode_grid_continuation_reduces_available_block_size_in_physical_x() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.writing_mode = WritingMode::VerticalLr;
  // Use three rows so the target item starts part-way into a continuation fragment (x=120px).
  // With a 100px fragmentainer block size, that leaves 80px remaining.
  grid_style.width = Some(Length::px(220.0));
  grid_style.height = Some(Length::px(100.0));
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  grid_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(60.0)),
    GridTrack::Length(Length::px(60.0)),
    GridTrack::Length(Length::px(100.0)),
  ];

  let item1_style = Arc::new({
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalLr;
    style
  });
  let item1 = BoxNode::new_block(item1_style, FormattingContextType::Block, vec![]);

  let item2_style = Arc::new({
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalLr;
    style
  });
  let item2 = BoxNode::new_block(item2_style, FormattingContextType::Block, vec![]);

  // Tests build `ComputedStyle` objects manually, so inheritance is not automatic. Grid items
  // inherit `writing-mode`, so set it explicitly to ensure the item's own layout uses the same
  // physical block axis as the grid container.
  let mut item3_style = ComputedStyle::default();
  item3_style.writing_mode = WritingMode::VerticalLr;
  item3_style.width_keyword = Some(IntrinsicSizeKeyword::FillAvailable);
  item3_style.height_keyword = Some(IntrinsicSizeKeyword::FillAvailable);
  let item3 = BoxNode::new_block(Arc::new(item3_style), FormattingContextType::Block, vec![]);

  let root_box = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item1, item2, item3],
  );
  let box_tree = BoxTree::new(root_box);
  let item3_id = box_tree.root.children.get(2).expect("third grid item").id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(100.0, 100.0), 0.0));
  let fragments = engine.layout_tree(&box_tree).expect("layout");

  // Sanity-check the unfragmented flow positions so the continuation heuristic has stable inputs.
  let unfragmented_engine = LayoutEngine::new(LayoutConfig::for_viewport(Size::new(100.0, 100.0)));
  let unfragmented = unfragmented_engine.layout_tree(&box_tree).expect("layout");
  let unfragmented_item3 = fragments_with_id(&unfragmented.root, item3_id);
  assert_eq!(unfragmented_item3.len(), 1);
  let unfragmented_width = unfragmented_item3[0].bounds.width();
  assert!(
    (unfragmented_width - 100.0).abs() < 0.1,
    "expected grid item 3 to fill the 100px row track before pagination (got width={unfragmented_width})"
  );
  assert!(
    (unfragmented_item3[0].bounds.x() - 120.0).abs() < 0.1,
    "expected grid item 3 to start in the third row (got x={})",
    unfragmented_item3[0].bounds.x()
  );

  let mut item3_fragment = None;
  for page in &fragments.additional_fragments {
    let matches = fragments_with_id(page, item3_id);
    if let Some(found) = matches.first() {
      item3_fragment = Some(*found);
      break;
    }
  }
  let item3_fragment = item3_fragment.expect("grid item 3 should appear in a continuation page");
  let item3_width = item3_fragment.bounds.width();
  let item3_height = item3_fragment.bounds.height();
  assert!(
    (item3_width - 80.0).abs() < 0.1,
    "expected continuation grid item to shrink to the remaining 80px of the fragmentainer (got {item3_width})"
  );
  assert!(
    (item3_height - 100.0).abs() < 0.1,
    "expected continuation grid item to retain its height when only block-axis width is reduced (got {item3_height})"
  );
}

#[test]
fn orthogonal_vertical_grid_continuation_uses_fragmentation_axis_hint() {
  // The root of the paginated document is horizontal-tb, so pagination fragments along physical Y.
  // The grid itself is vertical-lr (block axis physical X), meaning the fragmentation axis is
  // orthogonal to the grid's own block axis. The continuation relayout logic must use the
  // fragmentainer axes hint (physical Y) rather than the grid container's writing mode (physical X)
  // when applying the remaining fragmentainer space.
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.writing_mode = WritingMode::VerticalLr;
  grid_style.width = Some(Length::px(100.0));
  // Three 60/60/100px inline-axis tracks -> 220px physical height.
  grid_style.height = Some(Length::px(220.0));
  // Columns are the inline axis, which is physical Y in vertical writing modes.
  grid_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(60.0)),
    GridTrack::Length(Length::px(60.0)),
    GridTrack::Length(Length::px(100.0)),
  ];
  // Single block-axis track so the item's width stays stable.
  grid_style.grid_template_rows = vec![GridTrack::Length(Length::px(100.0))];

  let item1_style = Arc::new({
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalLr;
    style
  });
  let item1 = BoxNode::new_block(item1_style, FormattingContextType::Block, vec![]);

  let item2_style = Arc::new({
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalLr;
    style
  });
  let item2 = BoxNode::new_block(item2_style, FormattingContextType::Block, vec![]);

  let mut item3_style = ComputedStyle::default();
  // Tests build `ComputedStyle` objects manually, so inheritance is not automatic.
  item3_style.writing_mode = WritingMode::VerticalLr;
  item3_style.width_keyword = Some(IntrinsicSizeKeyword::FillAvailable);
  item3_style.height_keyword = Some(IntrinsicSizeKeyword::FillAvailable);
  let item3 = BoxNode::new_block(Arc::new(item3_style), FormattingContextType::Block, vec![]);

  let grid_box = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![item1, item2, item3],
  );

  let root_box = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![grid_box],
  );
  let box_tree = BoxTree::new(root_box);
  let grid_node = box_tree.root.children.get(0).expect("grid child");
  let item3_id = grid_node.children.get(2).expect("third grid item").id;

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(100.0, 100.0), 0.0));
  let fragments = engine.layout_tree(&box_tree).expect("layout");

  // Sanity-check the unfragmented flow positions.
  let unfragmented_engine = LayoutEngine::new(LayoutConfig::for_viewport(Size::new(100.0, 100.0)));
  let unfragmented = unfragmented_engine.layout_tree(&box_tree).expect("layout");
  let unfragmented_item3 = fragments_with_id(&unfragmented.root, item3_id);
  assert_eq!(unfragmented_item3.len(), 1);
  assert!(
    (unfragmented_item3[0].bounds.y() - 120.0).abs() < 0.1,
    "expected grid item 3 to start in the third column (got y={})",
    unfragmented_item3[0].bounds.y()
  );
  assert!(
    (unfragmented_item3[0].bounds.height() - 100.0).abs() < 0.1,
    "expected grid item 3 to fill its 100px column track before pagination (got height={})",
    unfragmented_item3[0].bounds.height()
  );

  let mut item3_fragment = None;
  for page in &fragments.additional_fragments {
    let matches = fragments_with_id(page, item3_id);
    if let Some(found) = matches.first() {
      item3_fragment = Some(*found);
      break;
    }
  }
  let item3_fragment = item3_fragment.expect("grid item 3 should appear in a continuation page");
  let item3_width = item3_fragment.bounds.width();
  let item3_height = item3_fragment.bounds.height();
  assert!(
    (item3_height - 80.0).abs() < 0.1,
    "expected continuation grid item to shrink to the remaining 80px of the fragmentainer (got {item3_height})"
  );
  assert!(
    (item3_width - 100.0).abs() < 0.1,
    "expected continuation relayout to apply remaining space on physical Y (height), not shrink width (got {item3_width})"
  );
}

#[test]
fn pagination_keeps_fragment_boundary_margins_separate() {
  let mut first_style = ComputedStyle::default();
  first_style.height = Some(Length::px(10.0));
  first_style.margin_bottom = Some(Length::px(30.0));

  let mut second_style = ComputedStyle::default();
  second_style.height = Some(Length::px(10.0));
  second_style.margin_top = Some(Length::px(40.0));
  second_style.break_before = BreakBetween::Page;

  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![first, second],
  );
  let box_tree = BoxTree::new(root);

  let config = LayoutConfig::for_pagination(Size::new(200.0, 60.0), 0.0);
  let engine = LayoutEngine::new(config);
  let fragments = engine.layout_tree(&box_tree).expect("layout");
  assert_eq!(fragments.additional_fragments.len(), 1);

  let first_page = &fragments.root;
  let second_page = &fragments.additional_fragments[0];

  let first_block = first_page
    .children
    .iter()
    .find(|c| matches!(c.content, FragmentContent::Block { .. }))
    .expect("first page block");
  let second_block = second_page
    .children
    .iter()
    .find(|c| matches!(c.content, FragmentContent::Block { .. }))
    .expect("second page block");

  assert!(
    (second_block.bounds.y() - 40.0).abs() < 0.1,
    "second page should honor its own top margin instead of inheriting a collapsed value"
  );
  let trailing_space = first_page.bounds.height() - first_block.bounds.max_y();
  assert!(
    trailing_space.abs() < 0.1,
    "forced breaks should truncate the prior block's bottom margin (trailing_space={trailing_space})"
  );
}

#[test]
fn unforced_page_break_truncates_collapsed_margins_between_siblings() {
  let mut first_style = ComputedStyle::default();
  first_style.height = Some(Length::px(30.0));
  first_style.margin_bottom = Some(Length::px(80.0));

  let mut second_style = ComputedStyle::default();
  second_style.height = Some(Length::px(30.0));
  second_style.margin_top = Some(Length::px(60.0));

  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![first, second],
  );
  let box_tree = BoxTree::new(root);

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, 100.0), 0.0));
  let fragments = engine.layout_tree(&box_tree).expect("layout");

  assert_eq!(
    fragments.additional_fragments.len(),
    1,
    "expected pagination overflow to produce 2 pages"
  );
  let first_page = &fragments.root;
  let second_page = &fragments.additional_fragments[0];

  let first_block = first_page
    .children
    .iter()
    .find(|c| matches!(c.content, FragmentContent::Block { .. }) && (c.bounds.height() - 30.0).abs() < 0.1)
    .expect("first page block");
  let second_block = second_page
    .children
    .iter()
    .find(|c| matches!(c.content, FragmentContent::Block { .. }) && (c.bounds.height() - 30.0).abs() < 0.1)
    .expect("second page block");

  let trailing_space = first_page.bounds.height() - first_block.bounds.max_y();
  assert!(
    trailing_space.abs() < 0.1,
    "unforced breaks should truncate collapsed adjoining margins (trailing_space={trailing_space})"
  );
  assert!(
    second_block.bounds.y().abs() < 0.1,
    "unforced breaks should truncate the following block's leading margin (y={})",
    second_block.bounds.y()
  );
}

#[test]
fn unforced_pagination_break_truncates_collapsed_margins() {
  // Two blocks whose adjoining margins collapse to 60px (max(60px, 20px)).
  //
  // Pick a fragmentainer size so block1 fits but the collapsed margin + block2 do not, forcing an
  // *unforced* pagination break inside the collapsed margin space.
  let mut first_style = ComputedStyle::default();
  first_style.height = Some(Length::px(20.0));
  first_style.margin_bottom = Some(Length::px(60.0));

  let mut second_style = ComputedStyle::default();
  second_style.height = Some(Length::px(20.0));
  second_style.margin_top = Some(Length::px(20.0));

  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![first, second],
  );
  let box_tree = BoxTree::new(root);

  let first_id = box_tree.root.children.get(0).expect("first child").id;
  let second_id = box_tree.root.children.get(1).expect("second child").id;

  let page_height = 60.0;
  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(200.0, page_height), 0.0));
  let fragments = engine.layout_tree(&box_tree).expect("layout");

  assert!(
    !fragments.additional_fragments.is_empty(),
    "expected content to paginate across at least two pages"
  );

  let first_page = &fragments.root;
  let second_page = &fragments.additional_fragments[0];

  let first_block = fragments_with_id(first_page, first_id);
  assert_eq!(first_block.len(), 1, "expected first block to render on page 1");
  assert!(
    fragments_with_id(second_page, first_id).is_empty(),
    "expected first block to not be duplicated/continued onto page 2"
  );

  let second_block = fragments_with_id(second_page, second_id);
  assert_eq!(second_block.len(), 1, "expected second block to render on page 2");
  assert!(
    fragments_with_id(first_page, second_id).is_empty(),
    "expected second block to not render on page 1"
  );

  let first_block = first_block[0];
  assert!(
    (first_block.bounds.height() - 20.0).abs() < 0.1,
    "sanity-check: first block should preserve its height"
  );
  let trailing_space = first_page.bounds.height() - first_block.bounds.max_y();
  assert!(
    trailing_space.abs() < 0.1,
    "unforced pagination break should truncate collapsed margins before the break (trailing_space={trailing_space})"
  );

  let second_block = second_block[0];
  assert!(
    (second_block.bounds.height() - 20.0).abs() < 0.1,
    "sanity-check: second block should preserve its height"
  );
  assert!(
    second_block.bounds.y().abs() < 0.1,
    "unforced pagination break should truncate collapsed margins after the break (y={})",
    second_block.bounds.y()
  );
}

#[test]
fn multicolumn_unforced_break_truncates_leading_margins() {
  let mut root_style = ComputedStyle::default();
  root_style.column_count = Some(2);
  root_style.column_gap = Length::px(0.0);
  root_style.width = Some(Length::px(200.0));

  let mut first_style = ComputedStyle::default();
  first_style.height = Some(Length::px(20.0));
  first_style.margin_bottom = Some(Length::px(60.0));

  let mut second_style = ComputedStyle::default();
  second_style.height = Some(Length::px(20.0));
  second_style.margin_top = Some(Length::px(20.0));

  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![first, second],
  );
  let box_tree = BoxTree::new(root);

  let engine = LayoutEngine::with_defaults();
  let fragments = engine.layout_tree(&box_tree).expect("layout");

  let mut block_fragments: Vec<_> = fragments
    .root
    .children
    .iter()
    .filter(|c| {
      matches!(c.content, FragmentContent::Block { .. }) && (c.bounds.height() - 20.0).abs() < 0.1
    })
    .collect();
  block_fragments.sort_by(|a, b| {
    a.bounds
      .x()
      .partial_cmp(&b.bounds.x())
      .unwrap_or(std::cmp::Ordering::Equal)
  });

  assert_eq!(
    block_fragments.len(),
    2,
    "expected one fragment per column for the two blocks"
  );
  let second_column = block_fragments[1];
  assert!(
    second_column.bounds.y().abs() < 0.1,
    "unforced column breaks should truncate the leading margin of the first in-flow block (y={})",
    second_column.bounds.y()
  );
}

#[test]
fn multicolumn_forced_break_preserves_leading_margins() {
  let mut root_style = ComputedStyle::default();
  root_style.column_count = Some(2);
  root_style.column_gap = Length::px(0.0);
  root_style.width = Some(Length::px(200.0));

  let mut first_style = ComputedStyle::default();
  first_style.height = Some(Length::px(20.0));

  let mut second_style = ComputedStyle::default();
  second_style.height = Some(Length::px(20.0));
  second_style.margin_top = Some(Length::px(20.0));
  second_style.break_before = BreakBetween::Column;

  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  let root = BoxNode::new_block(
    Arc::new(root_style),
    FormattingContextType::Block,
    vec![first, second],
  );
  let box_tree = BoxTree::new(root);

  let engine = LayoutEngine::with_defaults();
  let fragments = engine.layout_tree(&box_tree).expect("layout");

  let mut block_fragments: Vec<_> = fragments
    .root
    .children
    .iter()
    .filter(|c| {
      matches!(c.content, FragmentContent::Block { .. }) && (c.bounds.height() - 20.0).abs() < 0.1
    })
    .collect();
  block_fragments.sort_by(|a, b| {
    a.bounds
      .x()
      .partial_cmp(&b.bounds.x())
      .unwrap_or(std::cmp::Ordering::Equal)
  });

  assert_eq!(block_fragments.len(), 2, "expected both blocks to render");
  let second_column = block_fragments[1];
  assert!(
    (second_column.bounds.y() - 20.0).abs() < 0.1,
    "forced column breaks should preserve the leading margin of the next fragment (y={})",
    second_column.bounds.y()
  );
}

#[test]
fn sticky_offsets_apply_to_additional_fragments() {
  let mut spacer_style = ComputedStyle::default();
  spacer_style.height = Some(Length::px(150.0));
  let spacer = BoxNode::new_block(Arc::new(spacer_style), FormattingContextType::Block, vec![]);

  let mut sticky_style = ComputedStyle::default();
  sticky_style.position = Position::Sticky;
  sticky_style.top = InsetValue::Length(Length::px(0.0));
  sticky_style.height = Some(Length::px(20.0));
  let sticky = BoxNode::new_block(Arc::new(sticky_style), FormattingContextType::Block, vec![]);

  let mut tail_style = ComputedStyle::default();
  // Keep trailing content within the same fragmentainer as the sticky element so sticky movement
  // isn't clamped away by the fragment slice boundary.
  tail_style.height = Some(Length::px(30.0));
  let tail = BoxNode::new_block(Arc::new(tail_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![spacer, sticky, tail],
  );
  let box_tree = BoxTree::new(root);

  let engine = LayoutEngine::new(LayoutConfig::for_pagination(Size::new(100.0, 100.0), 0.0));
  let mut tree = engine.layout_tree(&box_tree).expect("layout tree");

  let (before_pos, fragment_index, _sticky_fragment) =
    sticky_abs_position(&tree).expect("sticky fragment present");
  assert!(
    fragment_index > 0,
    "sticky element should live in an additional fragment"
  );

  let scroll_y = before_pos.y + 10.0;

  let renderer = FastRender::new().expect("renderer");
  renderer.apply_sticky_offsets_for_tree(&mut tree, Point::new(0.0, scroll_y));

  let (after_pos, after_fragment_index, _) =
    sticky_abs_position(&tree).expect("sticky fragment after offsets");
  assert_eq!(fragment_index, after_fragment_index);
  assert!(
    after_pos.y != before_pos.y,
    "sticky fragment in additional fragment should be repositioned after applying offsets"
  );
}

fn sticky_abs_position<'a>(tree: &'a FragmentTree) -> Option<(Point, usize, &'a FragmentNode)> {
  let roots = std::iter::once((&tree.root, 0usize)).chain(
    tree
      .additional_fragments
      .iter()
      .enumerate()
      .map(|(idx, root)| (root, idx + 1)),
  );

  for (root, idx) in roots {
    if let Some((pos, node)) = find_sticky(root, Point::ZERO) {
      return Some((pos, idx, node));
    }
  }

  None
}

fn find_sticky<'a>(node: &'a FragmentNode, offset: Point) -> Option<(Point, &'a FragmentNode)> {
  let is_sticky = node
    .style
    .as_ref()
    .map(|s| s.position.is_sticky())
    .unwrap_or(false);

  let abs_pos = Point::new(offset.x + node.bounds.x(), offset.y + node.bounds.y());
  if is_sticky {
    return Some((abs_pos, node));
  }

  let child_offset = abs_pos;
  for child in node.children.iter() {
    if let Some(found) = find_sticky(child, child_offset) {
      return Some(found);
    }
  }

  None
}

#[test]
fn column_fragmentation_uses_column_width_for_layout() {
  let text = "Wrap this text inside columns";
  let text_node = BoxNode::new_text(Arc::new(ComputedStyle::default()), text.to_string());
  let root = BoxNode::new_block(
    Arc::new(ComputedStyle::default()),
    FormattingContextType::Block,
    vec![text_node],
  );
  let box_tree = BoxTree::new(root);

  let viewport = Size::new(320.0, 400.0);
  let base_engine = LayoutEngine::new(LayoutConfig::for_viewport(viewport));
  let base_tree = base_engine
    .layout_tree(&box_tree)
    .expect("layout without fragmentation");
  let base_lines = collect_lines(&base_tree.root);
  assert_eq!(
    base_lines.len(),
    1,
    "text should fit on one line without column fragmentation"
  );

  let fragmentation = FragmentationOptions::new(400.0).with_columns(2, 20.0);
  let engine =
    LayoutEngine::new(LayoutConfig::for_viewport(viewport).with_fragmentation(fragmentation));
  let tree = engine.layout_tree(&box_tree).expect("layout with columns");

  let expected_column_width = (viewport.width - 20.0) / 2.0;
  assert!((tree.root.bounds.width() - expected_column_width).abs() < 0.1);
  assert_eq!(tree.viewport_size().width, viewport.width);

  let column_lines = collect_lines(&tree.root);
  assert!(
    column_lines.len() > 1,
    "text should wrap when constrained to column width"
  );
}

#[test]
fn break_before_column_only_applies_in_column_context() {
  let lead = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 10.0), vec![]);
  let mut breaker_style = ComputedStyle::default();
  breaker_style.break_before = BreakBetween::Column;
  let breaker = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 10.0, 50.0, 10.0),
    vec![],
    Arc::new(breaker_style),
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 50.0, 20.0), vec![lead, breaker]);

  let page_boundaries =
    resolve_fragmentation_boundaries_with_context(&root, 100.0, FragmentationContext::Page)
      .unwrap();
  assert_eq!(
    page_boundaries.len(),
    2,
    "page context ignores column breaks"
  );

  let column_boundaries =
    resolve_fragmentation_boundaries_with_context(&root, 100.0, FragmentationContext::Column)
      .unwrap();
  assert!(
    column_boundaries.len() > 2,
    "column context should honor column-forced breaks"
  );
  assert!(
    column_boundaries
      .iter()
      .any(|pos| (*pos - 10.0).abs() < 0.1),
    "break should align with the second block's start"
  );
}

#[test]
fn abspos_break_before_after_are_ignored_in_page_fragmentation() {
  // CSS Break 3: break-before/after/inside do not apply to absolutely-positioned boxes
  // (they form parallel flows). Include `fixed` which is also absolutely-positioned.
  for position in [Position::Absolute, Position::Fixed] {
    let first =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 80.0, 20.0), 1, vec![]);
    let second =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 30.0, 80.0, 20.0), 2, vec![]);

    let mut abs_style = ComputedStyle::default();
    abs_style.position = position;
    abs_style.break_before = BreakBetween::Page;
    abs_style.break_after = BreakBetween::Page;

    let mut abspos =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 20.0, 80.0, 10.0), 3, vec![]);
    abspos.style = Some(Arc::new(abs_style));

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 80.0, 50.0),
      vec![first, abspos, second],
    );

    let boundaries =
      resolve_fragmentation_boundaries_with_context(&root, 100.0, FragmentationContext::Page)
        .unwrap();
    assert_eq!(
      boundaries.len(),
      2,
      "abspos break hints must not contribute forced boundaries (position={position:?}, boundaries={boundaries:?})"
    );

    let fragments = fragment_tree(&root, &FragmentationOptions::new(100.0)).unwrap();
    assert_eq!(
      fragments.len(),
      1,
      "abspos break hints must not force pagination boundaries (position={position:?})"
    );
    assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
    assert_eq!(fragments_with_id(&fragments[0], 2).len(), 1);
  }
}

#[test]
fn abspos_break_before_after_are_ignored_in_column_fragmentation() {
  for position in [Position::Absolute, Position::Fixed] {
    let first =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 0.0, 80.0, 20.0), 1, vec![]);
    let second =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 30.0, 80.0, 20.0), 2, vec![]);

    let mut abs_style = ComputedStyle::default();
    abs_style.position = position;
    abs_style.break_before = BreakBetween::Column;
    abs_style.break_after = BreakBetween::Column;

    let mut abspos =
      FragmentNode::new_block_with_id(Rect::from_xywh(0.0, 20.0, 80.0, 10.0), 3, vec![]);
    abspos.style = Some(Arc::new(abs_style));

    let root = FragmentNode::new_block(
      Rect::from_xywh(0.0, 0.0, 80.0, 50.0),
      vec![first, abspos, second],
    );

    let boundaries =
      resolve_fragmentation_boundaries_with_context(&root, 100.0, FragmentationContext::Column)
        .unwrap();
    assert_eq!(
      boundaries.len(),
      2,
      "abspos break hints must not contribute forced boundaries (position={position:?}, boundaries={boundaries:?})"
    );

    let options = FragmentationOptions::new(100.0).with_columns(2, 0.0);
    let fragments = fragment_tree(&root, &options).unwrap();
    assert_eq!(
      fragments.len(),
      1,
      "abspos break hints must not force column fragmentation boundaries (position={position:?})"
    );
    assert_eq!(fragments_with_id(&fragments[0], 1).len(), 1);
    assert_eq!(fragments_with_id(&fragments[0], 2).len(), 1);
  }
}

#[test]
fn table_headers_repeat_across_fragments() {
  let make = |display: Display, bounds: Rect, children: Vec<FragmentNode>| {
    let mut style = ComputedStyle::default();
    style.display = display;
    FragmentNode::new_block_styled(bounds, children, Arc::new(style))
  };

  let header_cell = make(
    Display::TableCell,
    Rect::from_xywh(0.0, 0.0, 100.0, 12.0),
    vec![],
  );
  let header_row = make(
    Display::TableRow,
    Rect::from_xywh(0.0, 0.0, 100.0, 12.0),
    vec![header_cell],
  );
  let header_group = make(
    Display::TableHeaderGroup,
    Rect::from_xywh(0.0, 0.0, 100.0, 12.0),
    vec![header_row],
  );

  let mut rows = Vec::new();
  let mut y = 12.0;
  for _ in 0..6 {
    let cell = make(
      Display::TableCell,
      Rect::from_xywh(0.0, 0.0, 100.0, 12.0),
      vec![],
    );
    let row = make(
      Display::TableRow,
      Rect::from_xywh(0.0, 0.0, 100.0, 12.0),
      vec![cell],
    );
    let row_group = make(
      Display::TableRowGroup,
      Rect::from_xywh(0.0, y, 100.0, 12.0),
      vec![row],
    );
    rows.push(row_group);
    y += 12.0;
  }

  let mut table_style = ComputedStyle::default();
  table_style.display = Display::Table;
  let mut table_children = Vec::new();
  table_children.push(header_group.clone());
  table_children.extend(rows);
  let table = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, y),
    table_children,
    Arc::new(table_style),
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, y), vec![table]);

  let fragments = fragment_tree(&root, &FragmentationOptions::new(36.0)).unwrap();
  assert!(fragments.len() >= 2, "table should fragment across pages");

  for fragment in &fragments {
    let header_count = fragment
      .iter_fragments()
      .filter(|node| {
        node
          .style
          .as_ref()
          .is_some_and(|style| matches!(style.display, Display::TableHeaderGroup))
      })
      .count();
    assert!(
      header_count >= 1,
      "each fragment should receive a repeated table header"
    );
  }
}

#[test]
fn table_headers_repeat_across_columns_without_overflow() {
  let make = |display: Display, bounds: Rect, children: Vec<FragmentNode>| {
    let mut style = ComputedStyle::default();
    style.display = display;
    FragmentNode::new_block_styled(bounds, children, Arc::new(style))
  };

  let header_cell = make(
    Display::TableCell,
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    vec![FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      "Header",
      16.0,
    )],
  );
  let header_row = make(
    Display::TableRow,
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    vec![header_cell],
  );
  let header_group = make(
    Display::TableHeaderGroup,
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    vec![header_row],
  );

  let mut rows = Vec::new();
  let mut y = 20.0;
  for idx in 1..=6 {
    let cell = make(
      Display::TableCell,
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      vec![FragmentNode::new_text(
        Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
        format!("Row {idx}"),
        16.0,
      )],
    );
    let row = make(
      Display::TableRow,
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      vec![cell],
    );
    let row_group = make(
      Display::TableRowGroup,
      Rect::from_xywh(0.0, y, 100.0, 20.0),
      vec![row],
    );
    rows.push(row_group);
    y += 20.0;
  }

  let mut table_style = ComputedStyle::default();
  table_style.display = Display::Table;
  let mut table_children = Vec::new();
  table_children.push(header_group.clone());
  table_children.extend(rows);
  let table = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, y),
    table_children,
    Arc::new(table_style),
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, y), vec![table]);

  let fragmentainer_size = 60.0;
  let options = FragmentationOptions::new(fragmentainer_size).with_columns(2, 0.0);
  let fragments = fragment_tree(&root, &options).unwrap();
  assert!(fragments.len() >= 2, "table should fragment across columns");

  for (idx, fragment) in fragments.iter().enumerate() {
    let header_count = fragment
      .iter_fragments()
      .filter(|node| {
        node
          .style
          .as_ref()
          .is_some_and(|style| matches!(style.display, Display::TableHeaderGroup))
      })
      .count();
    assert!(
      header_count >= 1,
      "fragment {idx} should include a repeated table header"
    );

    for row_group in fragment.iter_fragments().filter(|node| {
      node
        .style
        .as_ref()
        .is_some_and(|style| matches!(style.display, Display::TableRowGroup))
    }) {
      assert!(
        row_group.bounds.max_y() <= fragmentainer_size + 0.5,
        "table row group overflowed its column fragment (fragment {idx}, bottom={}, limit={})",
        row_group.bounds.max_y(),
        fragmentainer_size
      );
    }
  }
}

#[test]
fn table_footers_repeat_across_columns_without_overflow() {
  let make = |display: Display, bounds: Rect, children: Vec<FragmentNode>| {
    let mut style = ComputedStyle::default();
    style.display = display;
    FragmentNode::new_block_styled(bounds, children, Arc::new(style))
  };

  let header_cell = make(
    Display::TableCell,
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    vec![FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      "Header",
      16.0,
    )],
  );
  let header_row = make(
    Display::TableRow,
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    vec![header_cell],
  );
  let header_group = make(
    Display::TableHeaderGroup,
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    vec![header_row],
  );

  let mut rows = Vec::new();
  let mut y = 20.0;
  for idx in 1..=6 {
    let cell = make(
      Display::TableCell,
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      vec![FragmentNode::new_text(
        Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
        format!("Row {idx}"),
        16.0,
      )],
    );
    let row = make(
      Display::TableRow,
      Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
      vec![cell],
    );
    let row_group = make(
      Display::TableRowGroup,
      Rect::from_xywh(0.0, y, 100.0, 20.0),
      vec![row],
    );
    rows.push(row_group);
    y += 20.0;
  }

  let footer_cell = make(
    Display::TableCell,
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    vec![FragmentNode::new_text(
      Rect::from_xywh(0.0, 0.0, 50.0, 20.0),
      "Footer",
      16.0,
    )],
  );
  let footer_row = make(
    Display::TableRow,
    Rect::from_xywh(0.0, 0.0, 100.0, 20.0),
    vec![footer_cell],
  );
  let footer_group = make(
    Display::TableFooterGroup,
    Rect::from_xywh(0.0, y, 100.0, 20.0),
    vec![footer_row],
  );
  y += 20.0;

  let mut table_style = ComputedStyle::default();
  table_style.display = Display::Table;
  let mut table_children = Vec::new();
  table_children.push(header_group.clone());
  table_children.extend(rows);
  table_children.push(footer_group.clone());
  let table = FragmentNode::new_block_styled(
    Rect::from_xywh(0.0, 0.0, 100.0, y),
    table_children,
    Arc::new(table_style),
  );
  let root = FragmentNode::new_block(Rect::from_xywh(0.0, 0.0, 100.0, y), vec![table]);

  let fragmentainer_size = 60.0;
  let options = FragmentationOptions::new(fragmentainer_size).with_columns(2, 0.0);
  let fragments = fragment_tree(&root, &options).unwrap();
  assert!(fragments.len() >= 2, "table should fragment across columns");

  for (idx, fragment) in fragments.iter().enumerate() {
    let header_count = fragment
      .iter_fragments()
      .filter(|node| {
        node
          .style
          .as_ref()
          .is_some_and(|style| matches!(style.display, Display::TableHeaderGroup))
      })
      .count();
    let footer_count = fragment
      .iter_fragments()
      .filter(|node| {
        node
          .style
          .as_ref()
          .is_some_and(|style| matches!(style.display, Display::TableFooterGroup))
      })
      .count();
    let row_group_count = fragment
      .iter_fragments()
      .filter(|node| {
        node
          .style
          .as_ref()
          .is_some_and(|style| matches!(style.display, Display::TableRowGroup))
      })
      .count();

    assert!(
      row_group_count >= 1,
      "fragment {idx} should contain table rows"
    );
    assert!(
      header_count >= 1,
      "fragment {idx} should include a repeated table header"
    );
    assert!(
      footer_count >= 1,
      "fragment {idx} should include a repeated table footer"
    );

    for node in fragment.iter_fragments().filter(|node| {
      node.style.as_ref().is_some_and(|style| {
        matches!(
          style.display,
          Display::TableRowGroup | Display::TableFooterGroup
        )
      })
    }) {
      assert!(
        node.bounds.max_y() <= fragmentainer_size + 0.5,
        "table content overflowed its column fragment (fragment {idx}, bottom={}, limit={})",
        node.bounds.max_y(),
        fragmentainer_size
      );
    }
  }
}
