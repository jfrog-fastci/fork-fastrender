use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::{Direction, FlexDirection, JustifyContent, WritingMode};
use fastrender::style::values::Length;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode};
use fastrender::{BoxNode, ComputedStyle, FormattingContext, FormattingContextType};
use std::sync::Arc;
use taffy::prelude as taffy_prelude;

fn find_child_by_id<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  fragment.children.iter().find(|child| {
    matches!(
      child.content,
      FragmentContent::Block { box_id: Some(box_id) }
        | FragmentContent::Inline { box_id: Some(box_id), .. }
        | FragmentContent::Text { box_id: Some(box_id), .. }
        | FragmentContent::Replaced { box_id: Some(box_id), .. }
        if box_id == id
    )
  })
}

#[test]
fn taffy_sanity_row_reverse_flex_start_places_child_at_end() {
  use taffy_prelude::*;

  let mut taffy = TaffyTree::<()>::new();

  let child = taffy
    .new_leaf(Style {
      size: Size {
        width: Dimension::length(10.0),
        height: Dimension::length(10.0),
      },
      ..Default::default()
    })
    .expect("taffy leaf");

  let root = taffy
    .new_with_children(
      Style {
        display: Display::Flex,
        flex_direction: FlexDirection::RowReverse,
        justify_content: Some(JustifyContent::FlexStart),
        size: Size {
          width: Dimension::length(100.0),
          height: Dimension::length(10.0),
        },
        ..Default::default()
      },
      &[child],
    )
    .expect("taffy root");

  taffy
    .compute_layout(
      root,
      Size {
        width: AvailableSpace::Definite(100.0),
        height: AvailableSpace::Definite(10.0),
      },
    )
    .expect("taffy layout");

  let layout = taffy.layout(child).expect("taffy layout result");
  assert!(
    (layout.location.x - 90.0).abs() < 1e-3,
    "expected Taffy to place child at x=90 for row-reverse flex-start (got x={})",
    layout.location.x
  );
}

#[test]
fn justify_content_flex_start_row_reverse_is_not_double_inverted() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let mut container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);
  container.id = 100;

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 90.0).abs() < 1e-3,
    "expected flex-start on row-reverse to position child at the right edge (got x={}, width={}, container_x={}, container_width={})",
    child.bounds.x(),
    child.bounds.width(),
    fragment.bounds.x(),
    fragment.bounds.width()
  );
}

#[test]
fn justify_content_flex_start_rtl_row_is_not_double_inverted() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 90.0).abs() < 1e-3,
    "expected flex-start on rtl row to position child at the right edge (got x={}, width={}, container_x={}, container_width={})",
    child.bounds.x(),
    child.bounds.width(),
    fragment.bounds.x(),
    fragment.bounds.width()
  );
}

#[test]
fn justify_content_flex_start_column_reverse_is_not_double_inverted() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(10.0));
  container_style.height = Some(Length::px(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(10.0, 100.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.y() - 90.0).abs() < 1e-3,
    "expected flex-start on column-reverse to position child at the bottom edge (got y={})",
    child.bounds.y()
  );
}

#[test]
fn justify_content_flex_end_rtl_row_is_not_double_inverted() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::Row;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 0.0).abs() < 1e-3,
    "expected flex-end on rtl row to position child at the left edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_end_column_reverse_is_not_double_inverted() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = Some(Length::px(10.0));
  container_style.height = Some(Length::px(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(10.0, 100.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.y() - 0.0).abs() < 1e-3,
    "expected flex-end on column-reverse to position child at the top edge (got y={})",
    child.bounds.y()
  );
}

#[test]
fn justify_content_flex_start_rtl_row_reverse_is_not_double_inverted() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 0.0).abs() < 1e-3,
    "expected flex-start on rtl row-reverse to position child at the left edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_end_rtl_row_reverse_is_not_double_inverted() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.direction = Direction::Rtl;
  container_style.flex_direction = FlexDirection::RowReverse;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 90.0).abs() < 1e-3,
    "expected flex-end on rtl row-reverse to position child at the right edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_start_vertical_rl_column_aligns_to_block_start_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 90.0).abs() < 1e-3,
    "expected flex-start on vertical-rl column to align to block-start (right) edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_end_vertical_rl_column_aligns_to_block_end_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 0.0).abs() < 1e-3,
    "expected flex-end on vertical-rl column to align to block-end (left) edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_start_vertical_rl_column_reverse_aligns_to_block_end_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 0.0).abs() < 1e-3,
    "expected flex-start on vertical-rl column-reverse to align to block-end (left) edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_end_vertical_rl_column_reverse_aligns_to_block_start_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 90.0).abs() < 1e-3,
    "expected flex-end on vertical-rl column-reverse to align to block-start (right) edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_start_vertical_lr_column_aligns_to_block_start_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 0.0).abs() < 1e-3,
    "expected flex-start on vertical-lr column to align to block-start (left) edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_end_vertical_lr_column_aligns_to_block_end_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::Column;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 90.0).abs() < 1e-3,
    "expected flex-end on vertical-lr column to align to block-end (right) edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_start_vertical_lr_column_reverse_aligns_to_block_end_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 90.0).abs() < 1e-3,
    "expected flex-start on vertical-lr column-reverse to align to block-end (right) edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_end_vertical_lr_column_reverse_aligns_to_block_start_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalLr;
  container_style.flex_direction = FlexDirection::ColumnReverse;
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(10.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(100.0, 10.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.x() - 0.0).abs() < 1e-3,
    "expected flex-end on vertical-lr column-reverse to align to block-start (left) edge (got x={})",
    child.bounds.x()
  );
}

#[test]
fn justify_content_flex_start_vertical_rl_row_reverse_aligns_to_inline_end_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::RowReverse; // inline axis reversed (vertical)
  container_style.justify_content = JustifyContent::FlexStart;
  container_style.width = Some(Length::px(10.0));
  container_style.height = Some(Length::px(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(10.0, 100.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.y() - 90.0).abs() < 1e-3,
    "expected flex-start on vertical-rl row-reverse to align to inline-end (bottom) edge (got y={})",
    child.bounds.y()
  );
}

#[test]
fn justify_content_flex_end_vertical_rl_row_reverse_aligns_to_inline_start_edge() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = WritingMode::VerticalRl;
  container_style.flex_direction = FlexDirection::RowReverse; // inline axis reversed (vertical)
  container_style.justify_content = JustifyContent::FlexEnd;
  container_style.width = Some(Length::px(10.0));
  container_style.height = Some(Length::px(100.0));

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.width = Some(Length::px(10.0));
  child_style.height = Some(Length::px(10.0));
  child_style.flex_shrink = 0.0;
  let mut child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);
  child.id = 1;

  let container =
    BoxNode::new_block(Arc::new(container_style), FormattingContextType::Flex, vec![child]);

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(&container, &LayoutConstraints::definite(10.0, 100.0))
    .expect("layout succeeds");

  let child = find_child_by_id(&fragment, 1).unwrap_or_else(|| panic!("missing child"));
  assert!(
    (child.bounds.y() - 0.0).abs() < 1e-3,
    "expected flex-end on vertical-rl row-reverse to align to inline-start (top) edge (got y={})",
    child.bounds.y()
  );
}
