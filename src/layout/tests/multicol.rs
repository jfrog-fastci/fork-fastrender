use crate::debug::runtime::RuntimeToggles;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::block::BlockFormattingContext;
use crate::layout::engine::LayoutParallelism;
use crate::layout::formatting_context::{
  set_fragmentainer_block_offset_hint, set_fragmentainer_block_size_hint,
};
use crate::paint::display_list_builder::DisplayListBuilder;
use crate::paint::display_list_renderer::PaintParallelism;
use crate::style::color::Rgba;
use crate::style::display::{Display, FormattingContextType};
use crate::style::float::{Clear, Float};
use crate::style::types::BorderStyle;
use crate::style::types::BreakBetween;
use crate::style::types::BreakInside;
use crate::style::types::GridTrack;
use crate::style::types::ColumnFill;
use crate::style::types::ColumnSpan;
use crate::style::types::AlignItems;
use crate::style::types::FlexDirection;
use crate::style::types::WhiteSpace;
use crate::style::types::WritingMode;
use crate::style::values::Length;
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use crate::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use crate::FormattingContext;
use crate::{
  FastRender, FastRenderConfig, RenderArtifactRequest, RenderArtifacts, RenderOptions,
};
use std::collections::HashMap;
use std::sync::Arc;

fn find_fragment<'a>(fragment: &'a FragmentNode, id: usize) -> Option<&'a FragmentNode> {
  if let FragmentContent::Block { box_id: Some(b) } = fragment.content {
    if b == id {
      return Some(fragment);
    }
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_fragment(child, id) {
      return Some(found);
    }
  }
  None
}

fn find_rule_fragment<'a>(fragment: &'a FragmentNode, color: Rgba) -> Option<&'a FragmentNode> {
  if matches!(fragment.content, FragmentContent::Block { box_id: None })
    && fragment.style.as_ref().is_some_and(|s| {
      s.border_left_color == color
        && s.border_left_width.to_px() > 0.0
        && !matches!(s.border_left_style, BorderStyle::None | BorderStyle::Hidden)
    })
  {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_rule_fragment(child, color) {
      return Some(found);
    }
  }
  None
}

fn find_rule_fragment_with_color<'a>(
  fragment: &'a FragmentNode,
  color: Rgba,
) -> Option<&'a FragmentNode> {
  if matches!(fragment.content, FragmentContent::Block { box_id: None })
    && fragment.style.as_ref().is_some_and(|s| {
      (s.border_left_color == color && s.border_left_width.to_px() > 0.0)
        || (s.border_top_color == color && s.border_top_width.to_px() > 0.0)
    })
  {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_rule_fragment_with_color(child, color) {
      return Some(found);
    }
  }
  None
}

fn count_rule_fragments(fragment: &FragmentNode, color: Rgba) -> usize {
  fn walk(fragment: &FragmentNode, color: Rgba, count: &mut usize) {
    let is_rule = matches!(fragment.content, FragmentContent::Block { box_id: None })
      && fragment.style.as_ref().is_some_and(|s| {
        (s.border_left_color == color
          && s.border_left_width.to_px() > 0.0
          && !matches!(s.border_left_style, BorderStyle::None | BorderStyle::Hidden))
          || (s.border_top_color == color
            && s.border_top_width.to_px() > 0.0
            && !matches!(s.border_top_style, BorderStyle::None | BorderStyle::Hidden))
      });
    if is_rule {
      *count += 1;
    }
    for child in fragment.children.iter() {
      walk(child, color, count);
    }
  }

  let mut count = 0;
  walk(fragment, color, &mut count);
  count
}

fn collect_rule_fragments<'a>(fragment: &'a FragmentNode, color: Rgba) -> Vec<&'a FragmentNode> {
  fn walk<'a>(fragment: &'a FragmentNode, color: Rgba, out: &mut Vec<&'a FragmentNode>) {
    let is_rule = matches!(fragment.content, FragmentContent::Block { box_id: None })
      && fragment.style.as_ref().is_some_and(|s| {
        (s.border_left_color == color
          && s.border_left_width.to_px() > 0.0
          && !matches!(s.border_left_style, BorderStyle::None | BorderStyle::Hidden))
          || (s.border_top_color == color
            && s.border_top_width.to_px() > 0.0
            && !matches!(s.border_top_style, BorderStyle::None | BorderStyle::Hidden))
      });
    if is_rule {
      out.push(fragment);
    }
    for child in fragment.children.iter() {
      walk(child, color, out);
    }
  }

  let mut out = Vec::new();
  walk(fragment, color, &mut out);
  out
}

fn fragments_with_id<'a>(fragment: &'a FragmentNode, id: usize) -> Vec<&'a FragmentNode> {
  fn walk<'a>(fragment: &'a FragmentNode, id: usize, out: &mut Vec<&'a FragmentNode>) {
    if let FragmentContent::Block { box_id: Some(b) } = fragment.content {
      if b == id {
        out.push(fragment);
      }
    }
    for child in fragment.children.iter() {
      walk(child, id, out);
    }
  }

  let mut out = Vec::new();
  walk(fragment, id, &mut out);
  out
}

fn fragments_with_id_abs<'a>(
  fragment: &'a FragmentNode,
  id: usize,
) -> Vec<(f32, f32, &'a FragmentNode)> {
  fn walk<'a>(
    fragment: &'a FragmentNode,
    id: usize,
    origin: (f32, f32),
    out: &mut Vec<(f32, f32, &'a FragmentNode)>,
  ) {
    let abs = (origin.0 + fragment.bounds.x(), origin.1 + fragment.bounds.y());
    if let FragmentContent::Block { box_id: Some(b) } = fragment.content {
      if b == id {
        out.push((abs.0, abs.1, fragment));
      }
    }
    for child in fragment.children.iter() {
      walk(child, id, abs, out);
    }
  }

  let mut out = Vec::new();
  walk(fragment, id, (0.0, 0.0), &mut out);
  out
}

fn count_lines(fragment: &FragmentNode) -> usize {
  let mut count = 0;
  if matches!(fragment.content, FragmentContent::Line { .. }) {
    count += 1;
  }
  for child in fragment.children.iter() {
    count += count_lines(child);
  }
  count
}

fn collect_line_positions(fragment: &FragmentNode, origin: (f32, f32), out: &mut Vec<(f32, f32)>) {
  let current = (
    origin.0 + fragment.bounds.x(),
    origin.1 + fragment.bounds.y(),
  );
  if matches!(fragment.content, FragmentContent::Line { .. }) {
    out.push(current);
  }
  for child in fragment.children.iter() {
    collect_line_positions(child, current, out);
  }
}

fn first_fragment_absolute_position(fragment: &FragmentNode, id: usize) -> Option<(f32, f32)> {
  let mut stack = vec![(fragment, (0.0f32, 0.0f32))];
  while let Some((node, origin)) = stack.pop() {
    let current = (origin.0 + node.bounds.x(), origin.1 + node.bounds.y());
    if let FragmentContent::Block { box_id: Some(b) } = node.content {
      if b == id {
        return Some(current);
      }
    }
    for child in node.children.iter() {
      stack.push((child, current));
    }
  }
  None
}

fn layout_multicol_fragment(
  container_width: f32,
  column_gap: f32,
  column_count: u32,
  column_width: f32,
) -> (FragmentNode, usize) {
  let mut container_style = ComputedStyle::default();
  container_style.width = Some(Length::px(container_width));
  let count = column_count.max(0) as u32;
  container_style.column_count = Some(count);
  container_style.column_gap = Length::px(column_gap);
  container_style.column_width = Some(Length::px(column_width));
  let container_style = Arc::new(container_style);

  let mut child_style = ComputedStyle::default();
  child_style.height = Some(Length::px(20.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let mut container =
    BoxNode::new_block(container_style, FormattingContextType::Block, vec![child]);
  container.id = 900;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::definite_width(container_width),
    )
    .expect("layout");

  (fragment, container.id)
}

#[test]
fn column_count_is_treated_as_a_maximum() {
  let (fragment, container_id) = layout_multicol_fragment(250.0, 10.0, 3, 100.0);
  let container = find_fragment(&fragment, container_id).expect("multicol fragment");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");

  assert_eq!(info.column_count, 2);
  assert!((info.column_width - 120.0).abs() < 0.01);
}

#[test]
fn full_column_count_used_when_widths_fit() {
  let (fragment, container_id) = layout_multicol_fragment(330.0, 10.0, 3, 100.0);
  let container = find_fragment(&fragment, container_id).expect("multicol fragment");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");

  assert_eq!(info.column_count, 3);
  assert!((info.column_width - 103.33).abs() < 0.05);
  assert!(info.column_width >= 100.0);
}

#[test]
fn narrow_container_collapses_to_single_column() {
  let (fragment, container_id) = layout_multicol_fragment(180.0, 10.0, 3, 100.0);
  let container = find_fragment(&fragment, container_id).expect("multicol fragment");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");

  assert_eq!(info.column_count, 1);
  assert!((info.column_width - 180.0).abs() < 0.01);
}

#[test]
fn long_paragraph_splits_across_columns() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(300.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(20.0);
  let parent_style = Arc::new(parent_style);

  let mut para_style = ComputedStyle::default();
  para_style.white_space = WhiteSpace::Pre;
  let para_style = Arc::new(para_style);

  let text: String = (0..20).map(|i| format!("line {}\n", i)).collect();
  let mut para = BoxNode::new_block(
    para_style.clone(),
    FormattingContextType::Block,
    vec![BoxNode::new_text(para_style.clone(), text)],
  );
  para.id = 11;

  let mut parent = BoxNode::new_block(parent_style, FormattingContextType::Block, vec![para]);
  parent.id = 12;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(300.0))
    .expect("layout");

  let mut lines = Vec::new();
  collect_line_positions(&fragment, (0.0, 0.0), &mut lines);

  assert!(
    lines.iter().any(|(x, _)| *x >= 150.0),
    "lines should continue into the second column"
  );
}

#[test]
fn balanced_fill_spreads_lines_evenly() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(360.0));
  parent_style.column_count = Some(3);
  parent_style.column_gap = Length::px(10.0);
  let parent_style = Arc::new(parent_style);

  let mut para_style = ComputedStyle::default();
  para_style.white_space = WhiteSpace::Pre;
  let para_style = Arc::new(para_style);

  let text: String = (0..15).map(|i| format!("l{}\n", i)).collect();
  let para = BoxNode::new_block(
    para_style.clone(),
    FormattingContextType::Block,
    vec![BoxNode::new_text(para_style.clone(), text)],
  );

  let root = BoxNode::new_block(parent_style, FormattingContextType::Block, vec![para]);

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite_width(360.0))
    .expect("layout");

  let mut lines = Vec::new();
  collect_line_positions(&fragment, (0.0, 0.0), &mut lines);

  let column_width = (360.0 - 10.0 * 2.0) / 3.0;
  let stride = column_width + 10.0;
  let mut counts: HashMap<usize, usize> = HashMap::new();
  for (x, _) in lines {
    let col = ((x / stride).floor() as usize).min(4);
    *counts.entry(col).or_default() += 1;
  }

  assert_eq!(counts.len(), 3, "all columns should receive content");
  let min = counts.values().copied().min().unwrap_or(0);
  let max = counts.values().copied().max().unwrap_or(0);
  assert!(
    max.saturating_sub(min) <= 1,
    "balanced fill should distribute lines evenly (counts={counts:?})"
  );
}

#[test]
fn column_fill_auto_uses_definite_height() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(60.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(10.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  let child_style = |height: f32| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalRl;
    style.height = Some(Length::px(height));
    style.break_inside = BreakInside::AvoidColumn;
    Arc::new(style)
  };

  let mut first = BoxNode::new_block(child_style(40.0), FormattingContextType::Block, vec![]);
  first.id = 41;
  let mut second = BoxNode::new_block(child_style(40.0), FormattingContextType::Block, vec![]);
  second.id = 42;
  let mut third = BoxNode::new_block(child_style(40.0), FormattingContextType::Block, vec![]);
  third.id = 43;
  let mut fourth = BoxNode::new_block(child_style(40.0), FormattingContextType::Block, vec![]);
  fourth.id = 44;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![first.clone(), second.clone(), third.clone(), fourth.clone()],
  );
  parent.id = 40;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let first_frag = find_fragment(&fragment, first.id).expect("first fragment");
  let second_frag = find_fragment(&fragment, second.id).expect("second fragment");
  let third_frag = find_fragment(&fragment, third.id).expect("third fragment");
  let fourth_frag = find_fragment(&fragment, fourth.id).expect("fourth fragment");

  let container = find_fragment(&fragment, parent.id).expect("multicol container fragment");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");
  let stride = info.column_width + info.column_gap;

  assert!(
    (first_frag.bounds.y() - 0.0).abs() < 0.1,
    "first column should start at y=0"
  );
  assert!(
    (second_frag.bounds.y() - 0.0).abs() < 0.1,
    "second column should share the same set origin"
  );
  assert!(
    (third_frag.bounds.y() - 0.0).abs() < 0.1,
    "overflow content should start at the top of the overflow column"
  );
  assert!(
    (fourth_frag.bounds.y() - 0.0).abs() < 0.1,
    "additional overflow columns should also start at the top"
  );

  assert!(first_frag.bounds.x() < second_frag.bounds.x());
  assert!(second_frag.bounds.x() < third_frag.bounds.x());
  assert!(third_frag.bounds.x() < fourth_frag.bounds.x());

  assert!(
    third_frag.bounds.x() >= stride * 2.0 - 0.5,
    "overflow column should be placed in the inline direction"
  );
  assert!(
    fourth_frag.bounds.x() >= stride * 3.0 - 0.5,
    "additional overflow columns should continue in the inline direction"
  );
}

#[test]
fn column_fill_auto_balances_segment_before_spanner() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(400.0));
  parent_style.height = Some(Length::px(100.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(20.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  let child_style = |height: f32| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalRl;
    style.height = Some(Length::px(height));
    Arc::new(style)
  };

  let mut first = BoxNode::new_block(child_style(80.0), FormattingContextType::Block, vec![]);
  first.id = 1;
  let mut second = BoxNode::new_block(child_style(20.0), FormattingContextType::Block, vec![]);
  second.id = 2;

  let mut span_style = ComputedStyle::default();
  span_style.height = Some(Length::px(10.0));
  span_style.column_span = ColumnSpan::All;
  let mut span = BoxNode::new_block(Arc::new(span_style), FormattingContextType::Block, vec![]);
  span.id = 3;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![first.clone(), second.clone(), span.clone()],
  );
  parent.id = 10;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite(400.0, 100.0))
    .expect("layout");

  let span_frag = find_fragment(&fragment, span.id).expect("span fragment");
  assert!(
    (span_frag.bounds.y() - 80.0).abs() < 0.5,
    "spanner should start after a balanced segment (got y={})",
    span_frag.bounds.y()
  );
}

#[test]
fn column_span_all_splits_column_sets_in_fixed_height_container() {
  // Regression test for WPT `css/multicol/column-span-all-001`.
  //
  // The multicol container has a definite block-size (height). The column-set preceding a spanner
  // must be balanced based on the *content* extent, not the container's fixed height, so the
  // spanner is placed immediately after the balanced set.
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(100.0));
  parent_style.height = Some(Length::px(90.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  parent_style.column_fill = ColumnFill::Balance;
  let parent_style = Arc::new(parent_style);

  let item_style = |height: f32| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(height));
    Arc::new(style)
  };

  let mut before_left = BoxNode::new_block(item_style(20.0), FormattingContextType::Block, vec![]);
  before_left.id = 1;
  let mut before_right = BoxNode::new_block(item_style(20.0), FormattingContextType::Block, vec![]);
  before_right.id = 2;

  let mut span_style = ComputedStyle::default();
  span_style.height = Some(Length::px(10.0));
  span_style.column_span = ColumnSpan::All;
  let mut span = BoxNode::new_block(Arc::new(span_style), FormattingContextType::Block, vec![]);
  span.id = 3;

  let mut after_left = BoxNode::new_block(item_style(20.0), FormattingContextType::Block, vec![]);
  after_left.id = 4;
  let mut after_right = BoxNode::new_block(item_style(20.0), FormattingContextType::Block, vec![]);
  after_right.id = 5;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![
      before_left.clone(),
      before_right.clone(),
      span.clone(),
      after_left.clone(),
      after_right.clone(),
    ],
  );
  parent.id = 10;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite(100.0, 90.0))
    .expect("layout");

  let before_left_frag = find_fragment(&fragment, before_left.id).expect("before-left fragment");
  assert!((before_left_frag.bounds.x() - 0.0).abs() < 0.1);
  assert!((before_left_frag.bounds.y() - 0.0).abs() < 0.1);

  let before_right_frag = find_fragment(&fragment, before_right.id).expect("before-right fragment");
  assert!(
    (before_right_frag.bounds.x() - 50.0).abs() < 0.2,
    "expected before-right to be in the second column (x={})",
    before_right_frag.bounds.x()
  );
  assert!((before_right_frag.bounds.y() - 0.0).abs() < 0.1);

  let span_frag = find_fragment(&fragment, span.id).expect("spanner fragment");
  assert!(
    (span_frag.bounds.y() - 20.0).abs() < 0.3,
    "spanner should start after the balanced first column set (got y={})",
    span_frag.bounds.y()
  );
  assert!((span_frag.bounds.x()).abs() < 0.1);
  assert!(span_frag.bounds.width() > 99.0);

  let after_left_frag = find_fragment(&fragment, after_left.id).expect("after-left fragment");
  assert!((after_left_frag.bounds.x() - 0.0).abs() < 0.1);
  assert!(
    (after_left_frag.bounds.y() - 30.0).abs() < 0.3,
    "after-left should start below spanner (got y={})",
    after_left_frag.bounds.y()
  );

  let after_right_frag = find_fragment(&fragment, after_right.id).expect("after-right fragment");
  assert!(
    (after_right_frag.bounds.x() - 50.0).abs() < 0.2,
    "expected after-right to be in the second column (x={})",
    after_right_frag.bounds.x()
  );
  assert!(
    (after_right_frag.bounds.y() - 30.0).abs() < 0.3,
    "after-right should start below spanner (got y={})",
    after_right_frag.bounds.y()
  );
}

#[test]
fn column_rule_not_drawn_next_to_empty_columns() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(300.0));
  parent_style.height = Some(Length::px(100.0));
  parent_style.column_count = Some(3);
  parent_style.column_gap = Length::px(20.0);
  parent_style.column_fill = ColumnFill::Auto;
  parent_style.column_rule_style = BorderStyle::Solid;
  parent_style.column_rule_width = Length::px(6.0);
  let rule_color = Rgba::new(255, 0, 0, 1.0);
  parent_style.column_rule_color = Some(rule_color);
  let parent_style = Arc::new(parent_style);

  let mut child_style = ComputedStyle::default();
  child_style.height = Some(Length::px(20.0));
  child_style.break_after = BreakBetween::Column;
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(parent_style, FormattingContextType::Block, vec![child]);

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(300.0, 100.0))
    .expect("layout");

  assert_eq!(
    count_rule_fragments(&fragment, rule_color),
    0,
    "column rules should only be drawn between columns that both have content"
  );
}

#[test]
fn column_rules_only_between_adjacent_non_empty_columns() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(300.0));
  parent_style.height = Some(Length::px(100.0));
  parent_style.column_count = Some(3);
  parent_style.column_gap = Length::px(20.0);
  parent_style.column_fill = ColumnFill::Auto;
  parent_style.column_rule_style = BorderStyle::Solid;
  parent_style.column_rule_width = Length::px(6.0);
  let rule_color = Rgba::new(0, 0, 255, 1.0);
  parent_style.column_rule_color = Some(rule_color);
  let parent_style = Arc::new(parent_style);

  let child_style = |height: f32| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(height));
    style.break_after = BreakBetween::Column;
    Arc::new(style)
  };

  let first = BoxNode::new_block(child_style(40.0), FormattingContextType::Block, vec![]);
  let second = BoxNode::new_block(child_style(40.0), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![first, second],
  );

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(300.0, 100.0))
    .expect("layout");

  assert_eq!(
    count_rule_fragments(&fragment, rule_color),
    1,
    "expected exactly one rule between the two populated columns"
  );
}

#[test]
fn multicol_layout_balances_children_and_rules() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(400.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(20.0);
  parent_style.column_rule_style = BorderStyle::Solid;
  parent_style.column_rule_width = Length::px(6.0);
  let rule_color = Rgba::new(255, 0, 0, 1.0);
  parent_style.column_rule_color = Some(rule_color);
  let parent_style = Arc::new(parent_style);

  let child_style = |height: f32| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(height));
    Arc::new(style)
  };

  let mut first = BoxNode::new_block(child_style(80.0), FormattingContextType::Block, vec![]);
  first.id = 1;
  let mut second = BoxNode::new_block(child_style(80.0), FormattingContextType::Block, vec![]);
  second.id = 2;

  let mut span_style = ComputedStyle::default();
  span_style.height = Some(Length::px(30.0));
  span_style.column_span = ColumnSpan::All;
  let mut span = BoxNode::new_block(Arc::new(span_style), FormattingContextType::Block, vec![]);
  span.id = 3;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![first.clone(), second.clone(), span.clone()],
  );
  parent.id = 10;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(400.0))
    .expect("layout");

  let first_frag = find_fragment(&fragment, first.id).expect("first fragment");
  assert!((first_frag.bounds.x() - 0.0).abs() < 0.1);
  assert!((first_frag.bounds.y() - 0.0).abs() < 0.1);

  let second_frag = find_fragment(&fragment, second.id).expect("second fragment");
  assert!((second_frag.bounds.x() - 210.0).abs() < 0.2);
  assert!((second_frag.bounds.y() - 0.0).abs() < 0.1);

  let span_frag = find_fragment(&fragment, span.id).expect("span fragment");
  assert!((span_frag.bounds.x()).abs() < 0.1);
  assert!(span_frag.bounds.width() > 399.0);
  assert!((span_frag.bounds.y() - 80.0).abs() < 0.2);

  let rule_frag = find_rule_fragment(&fragment, rule_color).expect("column rule fragment");
  assert!((rule_frag.bounds.width() - 6.0).abs() < 0.1);
  assert!((rule_frag.bounds.height() - 80.0).abs() < 0.2);
  assert!((rule_frag.bounds.x() - 197.0).abs() < 0.5);
}

#[test]
fn column_rule_emits_dashed_border_item() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(260.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(18.0);
  parent_style.column_rule_style = BorderStyle::Dashed;
  parent_style.column_rule_width = Length::px(8.0);
  let rule_color = Rgba::new(0, 128, 0, 1.0);
  parent_style.column_rule_color = Some(rule_color);
  let parent_style = Arc::new(parent_style);

  let child_style = |height: f32| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(height));
    Arc::new(style)
  };

  let first = BoxNode::new_block(child_style(40.0), FormattingContextType::Block, vec![]);
  let second = BoxNode::new_block(child_style(40.0), FormattingContextType::Block, vec![]);

  let root = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![first, second],
  );

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite_width(260.0))
    .expect("layout");

  let display_list = DisplayListBuilder::new().build(&fragment);
  let border = display_list.items().iter().find_map(|item| {
    if let crate::DisplayItem::Border(border) = item {
      Some(border)
    } else {
      None
    }
  });

  let border = border.expect("column rule border item");
  assert_eq!(border.left.style, BorderStyle::Dashed);
  assert!((border.left.width - 8.0).abs() < 0.1);
  assert_eq!(border.left.color, rule_color);
}

#[test]
fn column_rule_uses_top_border_in_vertical_writing_mode() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(200.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(20.0);
  parent_style.column_rule_style = BorderStyle::Solid;
  parent_style.column_rule_width = Length::px(6.0);
  let rule_color = Rgba::new(255, 0, 0, 1.0);
  parent_style.column_rule_color = Some(rule_color);
  parent_style.writing_mode = WritingMode::VerticalRl;
  let parent_style = Arc::new(parent_style);

  let child_style = |height: f32, break_after: BreakBetween| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.writing_mode = WritingMode::VerticalRl;
    style.height = Some(Length::px(height));
    style.break_after = break_after;
    Arc::new(style)
  };

  let mut first = BoxNode::new_block(
    child_style(60.0, BreakBetween::Column),
    FormattingContextType::Block,
    vec![],
  );
  first.id = 1;
  let mut second = BoxNode::new_block(
    child_style(60.0, BreakBetween::Auto),
    FormattingContextType::Block,
    vec![],
  );
  second.id = 2;
  let (first_id, second_id) = (first.id, second.id);

  let root = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![first, second],
  );

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout");

  let first_frag = find_fragment(&fragment, first_id).expect("first fragment");
  let second_frag = find_fragment(&fragment, second_id).expect("second fragment");
  assert_eq!(
    first_frag.fragment_count, 2,
    "expected both fragments to be in a 2-column flow: {first_frag:?}"
  );
  assert_eq!(
    second_frag.fragment_count, 2,
    "expected both fragments to be in a 2-column flow: {second_frag:?}"
  );
  assert_ne!(
    first_frag.fragment_index, second_frag.fragment_index,
    "expected content to be distributed across columns (first idx={} bounds={:?}; second idx={} bounds={:?})",
    first_frag.fragment_index,
    first_frag.logical_override.unwrap_or(first_frag.bounds),
    second_frag.fragment_index,
    second_frag.logical_override.unwrap_or(second_frag.bounds)
  );

  let rule_frag =
    find_rule_fragment_with_color(&fragment, rule_color).expect("column rule fragment");
  let style = rule_frag.style.as_ref().expect("rule style");
  assert!(
    style.border_top_width.to_px() > 0.0,
    "column rule should paint using the block-start border in vertical writing modes"
  );
  assert_eq!(style.border_top_style, BorderStyle::Solid);
  assert_eq!(style.border_top_color, rule_color);
  assert_eq!(style.border_left_width.to_px(), 0.0);
  assert!(matches!(
    style.border_left_style,
    BorderStyle::None | BorderStyle::Hidden
  ));
}

#[test]
fn column_span_creates_new_segment() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(400.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(20.0);
  let parent_style = Arc::new(parent_style);

  let child_style = |height: f32| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(height));
    Arc::new(style)
  };

  let mut first = BoxNode::new_block(child_style(60.0), FormattingContextType::Block, vec![]);
  first.id = 1;
  let mut second = BoxNode::new_block(child_style(60.0), FormattingContextType::Block, vec![]);
  second.id = 2;

  let mut span_style = ComputedStyle::default();
  span_style.height = Some(Length::px(30.0));
  span_style.column_span = ColumnSpan::All;
  let mut span = BoxNode::new_block(Arc::new(span_style), FormattingContextType::Block, vec![]);
  span.id = 3;

  let mut trailing = BoxNode::new_block(child_style(40.0), FormattingContextType::Block, vec![]);
  trailing.id = 4;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![
      first.clone(),
      second.clone(),
      span.clone(),
      trailing.clone(),
    ],
  );
  parent.id = 20;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(400.0))
    .expect("layout");

  let span_frag = find_fragment(&fragment, span.id).expect("span fragment");
  assert!((span_frag.bounds.y() - 60.0).abs() < 0.5);

  let trailing_frag = find_fragment(&fragment, trailing.id).expect("trailing fragment");
  assert!(trailing_frag.bounds.y() >= span_frag.bounds.max_y());
}

#[test]
fn nested_multicol_layouts_columns() {
  let mut inner_style = ComputedStyle::default();
  inner_style.width = Some(Length::px(240.0));
  inner_style.column_count = Some(2);
  inner_style.column_gap = Length::px(16.0);
  let inner_style = Arc::new(inner_style);

  let child_style = |height: f32| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(height));
    Arc::new(style)
  };

  let mut inner_child1 =
    BoxNode::new_block(child_style(30.0), FormattingContextType::Block, vec![]);
  inner_child1.id = 5;
  let mut inner_child2 =
    BoxNode::new_block(child_style(30.0), FormattingContextType::Block, vec![]);
  inner_child2.id = 6;

  let mut inner = BoxNode::new_block(
    inner_style,
    FormattingContextType::Block,
    vec![inner_child1.clone(), inner_child2.clone()],
  );
  inner.id = 30;

  let outer_style = Arc::new(ComputedStyle::default());
  let root = BoxNode::new_block(
    outer_style,
    FormattingContextType::Block,
    vec![inner.clone()],
  );

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite_width(400.0))
    .expect("layout");

  let first_frag = find_fragment(&fragment, inner_child1.id).expect("first fragment");
  let second_frag = find_fragment(&fragment, inner_child2.id).expect("second fragment");

  assert!(first_frag.bounds.x() < second_frag.bounds.x());
  assert!((second_frag.bounds.x() - first_frag.bounds.x()) > 60.0);
  assert!((first_frag.bounds.y() - second_frag.bounds.y()).abs() < 0.1);
}

#[test]
fn break_before_column_advances_column() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  let parent_style = Arc::new(parent_style);

  let child_style = |break_before: Option<BreakBetween>, height: f32| -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(height));
    if let Some(b) = break_before {
      style.break_before = b;
    }
    Arc::new(style)
  };

  let mut first = BoxNode::new_block(
    child_style(None, 20.0),
    FormattingContextType::Block,
    vec![],
  );
  first.id = 50;
  let mut second = BoxNode::new_block(
    child_style(Some(BreakBetween::Column), 10.0),
    FormattingContextType::Block,
    vec![],
  );
  second.id = 51;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![first.clone(), second.clone()],
  );
  parent.id = 52;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let first_frag = find_fragment(&fragment, first.id).expect("first fragment");
  let second_frag = find_fragment(&fragment, second.id).expect("second fragment");

  assert!(first_frag.bounds.x() < 0.1);
  assert!(second_frag.bounds.x() > 90.0);
  assert!(second_frag.bounds.y() < 0.1);
}

#[test]
fn avoid_column_blocks_stay_whole() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  let parent_style = Arc::new(parent_style);

  let mut child_style = ComputedStyle::default();
  child_style.height = Some(Length::px(80.0));
  child_style.break_inside = BreakInside::AvoidColumn;
  let child_style = Arc::new(child_style);

  let mut child = BoxNode::new_block(child_style, FormattingContextType::Block, vec![]);
  child.id = 77;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![child.clone()],
  );
  parent.id = 78;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let child_frags = fragments_with_id(&fragment, child.id);
  assert_eq!(
    child_frags.len(),
    1,
    "avoid-column block should not split across columns"
  );
  assert!(
    (child_frags[0].bounds.height() - 80.0).abs() < 0.1,
    "avoided block should keep its full height"
  );
}

#[test]
fn avoid_column_flex_item_moves_to_next_column() {
  // Multi-column container with a definite height so each column is exactly 100px tall.
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(100.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  let flex_style = Arc::new(flex_style);

  let mut item1_style = ComputedStyle::default();
  item1_style.height = Some(Length::px(70.0));
  // Ensure the authored block-size is honoured as the flex base size.
  item1_style.flex_shrink = 0.0;
  let mut item1 = BoxNode::new_block(
    Arc::new(item1_style),
    FormattingContextType::Block,
    vec![],
  );
  item1.id = 700;

  // The second item is 60px tall (two 30px children). Without `break-inside: avoid-column`, the
  // multicol fragmentation boundary would prefer splitting at the internal 30px boundary (y=100).
  let mut item2_style = ComputedStyle::default();
  item2_style.break_inside = BreakInside::AvoidColumn;
  item2_style.flex_shrink = 0.0;
  let item2_style = Arc::new(item2_style);

  let mut inner_style = ComputedStyle::default();
  inner_style.height = Some(Length::px(30.0));
  let inner_style = Arc::new(inner_style);
  let inner_a = BoxNode::new_block(inner_style.clone(), FormattingContextType::Block, vec![]);
  let inner_b = BoxNode::new_block(inner_style.clone(), FormattingContextType::Block, vec![]);

  let mut item2 = BoxNode::new_block(
    item2_style,
    FormattingContextType::Block,
    vec![inner_a, inner_b],
  );
  item2.id = 701;

  let mut flex = BoxNode::new_block(
    flex_style,
    FormattingContextType::Flex,
    vec![item1.clone(), item2.clone()],
  );
  flex.id = 702;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![flex.clone()],
  );
  parent.id = 703;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  // The flex container should fragment across the two columns.
  let flex_frags = fragments_with_id(&fragment, flex.id);
  assert!(
    flex_frags.len() >= 2,
    "expected flex container to fragment across columns"
  );
  let mut flex_frags_sorted = flex_frags.clone();
  flex_frags_sorted.sort_by(|a, b| a.bounds.x().partial_cmp(&b.bounds.x()).unwrap());
  let first_column_flex_frag = flex_frags_sorted[0];

  // The avoid-column flex item fits in a column, so boundary selection should clamp the first
  // column to end *before* the item (at y=70), rather than breaking at the internal y=100 boundary.
  assert!(
    (first_column_flex_frag.bounds.height() - 70.0).abs() < 0.1,
    "expected first-column flex fragment to end at the first item boundary (70px) under avoid-column; got {}",
    first_column_flex_frag.bounds.height()
  );

  // The avoided flex item must not be split across columns.
  let item2_frags = fragments_with_id(&fragment, item2.id);
  assert_eq!(
    item2_frags.len(),
    1,
    "avoid-column flex item should not split across columns"
  );

  // The avoided flex item should be placed entirely in column 2.
  assert_eq!(
    item2_frags[0].fragmentainer.column_index,
    Some(1),
    "expected avoided flex item to be placed in column 2"
  );

  // Extra sanity: ensure the absolute x position differs from the first item (column 1).
  let item1_pos = first_fragment_absolute_position(&fragment, item1.id).expect("item1 position");
  let item2_pos = first_fragment_absolute_position(&fragment, item2.id).expect("item2 position");
  assert!(
    item2_pos.0 > item1_pos.0 + 50.0,
    "expected item2 to be in a later column: item1 at x={}, item2 at x={}",
    item1_pos.0,
    item2_pos.0
  );
}

#[test]
fn grid_item_avoid_column_moves_to_next_column_when_it_fits() {
  // Regression: grid items with `break-inside: avoid-column` should be treated as atomic and moved
  // to the next column when they fit there, instead of being clipped at the column boundary.
  let mut multicol_style = ComputedStyle::default();
  multicol_style.width = Some(Length::px(200.0));
  multicol_style.height = Some(Length::px(60.0));
  multicol_style.column_count = Some(2);
  multicol_style.column_gap = Length::px(0.0);
  multicol_style.column_fill = ColumnFill::Auto;
  let multicol_style = Arc::new(multicol_style);

  // Create a grid where the only item is placed in the second row. The second row track is larger
  // than the column height (60px) so track-level atomicity cannot prevent clipping; the
  // `break-inside` hint on the grid item must be respected.
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(200.0));
  grid_style.height = Some(Length::px(90.0));
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(200.0))];
  grid_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(120.0)),
  ];
  grid_style.align_items = AlignItems::Start;
  let grid_style = Arc::new(grid_style);

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  item_style.height = Some(Length::px(40.0));
  item_style.grid_row_start = 2;
  item_style.grid_row_end = 3;
  item_style.break_inside = BreakInside::AvoidColumn;
  let item_style = Arc::new(item_style);

  let mut item = BoxNode::new_block(item_style, FormattingContextType::Block, vec![]);
  item.id = 501;
  let item_id = item.id;

  let mut grid = BoxNode::new_block(grid_style, FormattingContextType::Grid, vec![item]);
  grid.id = 502;

  let mut multicol = BoxNode::new_block(multicol_style, FormattingContextType::Block, vec![grid]);
  multicol.id = 503;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&multicol, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let item_frags = fragments_with_id_abs(&fragment, item_id);
  assert_eq!(
    item_frags.len(),
    1,
    "avoid-column grid item should not split across columns (got fragments={:?})",
    item_frags
      .iter()
      .map(|(x, y, f)| (*x, *y, f.fragment_index, f.bounds))
      .collect::<Vec<_>>()
  );
  let (abs_x, abs_y, frag) = item_frags[0];
  assert!(
    (frag.bounds.height() - 40.0).abs() < 0.1,
    "grid item should keep its full height when moved (got h={})",
    frag.bounds.height()
  );
  assert!(
    abs_y.abs() < 0.1,
    "grid item should start at the top of the next column (got y={})",
    abs_y
  );
  assert!(
    abs_x > 90.0,
    "expected grid item to be in the second column (x={})",
    abs_x
  );
}

#[test]
fn avoid_flex_item_moves_to_next_column() {
  // `break-inside: avoid` should behave like `avoid-column` in a column fragmentation context.
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(100.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  let flex_style = Arc::new(flex_style);

  let mut item1_style = ComputedStyle::default();
  item1_style.height = Some(Length::px(70.0));
  item1_style.flex_shrink = 0.0;
  let mut item1 = BoxNode::new_block(
    Arc::new(item1_style),
    FormattingContextType::Block,
    vec![],
  );
  item1.id = 710;

  let mut item2_style = ComputedStyle::default();
  item2_style.break_inside = BreakInside::Avoid;
  item2_style.flex_shrink = 0.0;
  let item2_style = Arc::new(item2_style);

  let mut inner_style = ComputedStyle::default();
  inner_style.height = Some(Length::px(30.0));
  let inner_style = Arc::new(inner_style);
  let inner_a = BoxNode::new_block(inner_style.clone(), FormattingContextType::Block, vec![]);
  let inner_b = BoxNode::new_block(inner_style.clone(), FormattingContextType::Block, vec![]);

  let mut item2 = BoxNode::new_block(
    item2_style,
    FormattingContextType::Block,
    vec![inner_a, inner_b],
  );
  item2.id = 711;

  let mut flex = BoxNode::new_block(
    flex_style,
    FormattingContextType::Flex,
    vec![item1.clone(), item2.clone()],
  );
  flex.id = 712;

  let parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![flex.clone()],
  );

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let item2_frags = fragments_with_id(&fragment, item2.id);
  assert_eq!(
    item2_frags.len(),
    1,
    "break-inside: avoid flex item should not split across columns when it fits"
  );
  assert_eq!(
    item2_frags[0].fragmentainer.column_index,
    Some(1),
    "expected avoided flex item to be placed in column 2"
  );
}

#[test]
fn tall_avoid_column_flex_item_may_fragment() {
  // `break-inside: avoid-column` should not prevent fragmentation when the flex item is taller
  // than the column fragmentainer.
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(100.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  let flex_style = Arc::new(flex_style);

  let mut item_style = ComputedStyle::default();
  item_style.break_inside = BreakInside::AvoidColumn;
  item_style.flex_shrink = 0.0;
  let item_style = Arc::new(item_style);

  // 120px tall item inside 100px-tall columns, with a legal internal break at 60px so the engine
  // can split it.
  let mut inner_style = ComputedStyle::default();
  inner_style.height = Some(Length::px(60.0));
  let inner_style = Arc::new(inner_style);
  let inner_a = BoxNode::new_block(inner_style.clone(), FormattingContextType::Block, vec![]);
  let inner_b = BoxNode::new_block(inner_style.clone(), FormattingContextType::Block, vec![]);

  let mut item =
    BoxNode::new_block(item_style, FormattingContextType::Block, vec![inner_a, inner_b]);
  item.id = 720;

  let flex = BoxNode::new_block(
    flex_style,
    FormattingContextType::Flex,
    vec![item.clone()],
  );
  let parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![flex.clone()],
  );

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let item_frags = fragments_with_id(&fragment, item.id);
  assert!(
    item_frags.len() >= 2,
    "expected tall avoid-column flex item to fragment across columns"
  );
  let columns: std::collections::HashSet<_> = item_frags
    .iter()
    .map(|frag| frag.fragmentainer.column_index)
    .collect();
  assert!(
    columns.contains(&Some(0)) && columns.contains(&Some(1)),
    "expected fragments to span at least columns 0 and 1, got {columns:?}"
  );
}

#[test]
fn float_that_fits_is_not_split_or_clipped_across_columns() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(60.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  // Push the float down so the nominal column boundary lands in the middle of it.
  let mut leading_style = ComputedStyle::default();
  leading_style.height = Some(Length::px(30.0));
  // Penalize the boundary at the float start so the analyzer prefers a later line break that
  // overlaps the float when the float is not treated as atomic.
  leading_style.break_after = BreakBetween::AvoidColumn;
  let mut leading = BoxNode::new_block(
    Arc::new(leading_style),
    FormattingContextType::Block,
    vec![],
  );
  leading.id = 80;

  let mut float_style = ComputedStyle::default();
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(40.0));
  float_style.height = Some(Length::px(40.0));
  let mut float_node =
    BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
  float_node.id = 81;

  let mut para_style = ComputedStyle::default();
  para_style.white_space = WhiteSpace::Pre;
  para_style.orphans = 1;
  para_style.widows = 1;
  let para_style = Arc::new(para_style);
  let text: String = (0..8).map(|i| format!("line {}\n", i)).collect();
  let text_node = BoxNode::new_text(para_style.clone(), text);
  let mut para = BoxNode::new_block(
    para_style.clone(),
    FormattingContextType::Block,
    vec![text_node],
  );
  para.id = 82;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![leading.clone(), float_node.clone(), para.clone()],
  );
  parent.id = 83;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let float_frags = fragments_with_id(&fragment, float_node.id);
  assert_eq!(
    float_frags.len(),
    1,
    "float should be kept intact when it fits in a column (got fragments={:?})",
    float_frags
      .iter()
      .map(|f| (f.fragment_index, f.bounds))
      .collect::<Vec<_>>()
  );
  let float_frag = float_frags[0];
  assert!(
    (float_frag.bounds.height() - 40.0).abs() < 0.1,
    "float should not be clipped (got h={})",
    float_frag.bounds.height()
  );
  assert!(
    float_frag.bounds.max_y() <= 60.0 + 0.2,
    "float should fit wholly within a single column (bounds={:?})",
    float_frag.bounds
  );
}

#[test]
fn tall_float_fragments_across_columns_and_clear_respects_fragmented_extent() {
  let container_height = 60.0;
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(container_height));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  let mut float_style = ComputedStyle::default();
  float_style.float = Float::Left;
  float_style.width = Some(Length::px(200.0));
  float_style.height = Some(Length::px(140.0));
  let mut float_node =
    BoxNode::new_block(Arc::new(float_style), FormattingContextType::Block, vec![]);
  float_node.id = 200;

  let mut after_style = ComputedStyle::default();
  after_style.clear = Clear::Both;
  after_style.margin_top = Some(Length::px(0.0));
  after_style.margin_bottom = Some(Length::px(0.0));
  let after_style = Arc::new(after_style);
  let text_node = BoxNode::new_text(after_style.clone(), "After".to_string());
  let mut after =
    BoxNode::new_block(after_style, FormattingContextType::Block, vec![text_node]);
  after.id = 201;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![float_node.clone(), after.clone()],
  );
  parent.id = 202;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let container = find_fragment(&fragment, parent.id).expect("multicol container fragment");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");
  let stride = info.column_width + info.column_gap;
  let flow_height = container_height;

  let float_frags = fragments_with_id(&fragment, float_node.id);
  assert!(
    float_frags.len() >= 2,
    "expected tall float to fragment across columns (got fragments={:?})",
    float_frags
      .iter()
      .map(|f| (f.fragment_index, f.bounds))
      .collect::<Vec<_>>()
  );

  let mut indices: Vec<usize> = float_frags.iter().map(|f| f.fragment_index).collect();
  indices.sort_unstable();
  indices.dedup();
  assert!(
    indices.len() >= 2,
    "expected float fragments to have distinct fragment_index values (indices={indices:?}, frags={:?})",
    float_frags
      .iter()
      .map(|f| (f.fragment_index, f.bounds))
      .collect::<Vec<_>>()
  );

  let first = float_frags
    .iter()
    .find(|f| f.fragment_index == 0)
    .expect("float fragment in first column");
  let continuation = float_frags
    .iter()
    .find(|f| f.fragment_index >= 1)
    .expect("float continuation fragment");

  assert!(
    continuation.bounds.x() >= first.bounds.x() + stride - 0.5,
    "expected float continuation to be placed in a subsequent column (stride={stride}, first={:?}, cont={:?})",
    (first.fragment_index, first.bounds),
    (continuation.fragment_index, continuation.bounds)
  );
  assert!(
    (continuation.bounds.y() - 0.0).abs() < 0.2,
    "expected float continuation to start at the top of its column (got y={})",
    continuation.bounds.y()
  );

  let after_frags = fragments_with_id(&fragment, after.id);
  assert_eq!(
    after_frags.len(),
    1,
    "expected cleared content to appear only once (got fragments={:?})",
    after_frags
      .iter()
      .map(|f| (f.fragment_index, f.bounds))
      .collect::<Vec<_>>()
  );
  let after_frag = after_frags[0];

  let float_flow_bottom = float_frags
    .iter()
    .map(|f| (f.fragment_index as f32) * flow_height + f.bounds.max_y())
    .fold(f32::NEG_INFINITY, f32::max);
  let after_flow_top = (after_frag.fragment_index as f32) * flow_height + after_frag.bounds.y();

  assert!(
    after_flow_top + 0.2 >= float_flow_bottom,
    "expected cleared content to be laid out after the final float fragment (after_flow_top={}, float_flow_bottom={}, after={:?}, float_frags={:?})",
    after_flow_top,
    float_flow_bottom,
    (after_frag.fragment_index, after_frag.bounds),
    float_frags
      .iter()
      .map(|f| (f.fragment_index, f.bounds))
      .collect::<Vec<_>>()
  );
}

#[test]
fn avoid_column_flex_items_stay_whole_when_they_fit_in_next_column() {
  // Regression test: `break-inside: avoid-column` on flex items should prevent the item from being
  // split/clipped when it would fit in the next column.
  //
  // The avoided flex item contains two 20px children, creating a break opportunity at y=50 which
  // is closer to the 60px column boundary than the flex item's start at y=30. Without honouring
  // `break-inside: avoid-column` on the flex item, the fragmentation analyzer could split the item
  // at y=50 instead of moving it entirely to the next column.
  let mut multicol_style = ComputedStyle::default();
  multicol_style.width = Some(Length::px(200.0));
  multicol_style.height = Some(Length::px(60.0));
  multicol_style.column_count = Some(2);
  multicol_style.column_gap = Length::px(0.0);
  multicol_style.column_fill = ColumnFill::Auto;
  let multicol_style = Arc::new(multicol_style);

  let mut flex_style = ComputedStyle::default();
  flex_style.display = Display::Flex;
  flex_style.flex_direction = FlexDirection::Column;
  // Avoid flexing so item positions are driven by the authored heights.
  flex_style.flex_wrap = crate::style::types::FlexWrap::NoWrap;
  let flex_style = Arc::new(flex_style);

  let mut spacer_style = ComputedStyle::default();
  spacer_style.display = Display::Block;
  spacer_style.height = Some(Length::px(30.0));
  spacer_style.flex_shrink = 0.0;
  let spacer_style = Arc::new(spacer_style);
  let mut spacer = BoxNode::new_block(spacer_style, FormattingContextType::Block, vec![]);
  spacer.id = 201;

  let mut avoid_style = ComputedStyle::default();
  avoid_style.display = Display::Block;
  avoid_style.height = Some(Length::px(40.0));
  avoid_style.break_inside = BreakInside::AvoidColumn;
  avoid_style.flex_shrink = 0.0;
  let avoid_style = Arc::new(avoid_style);

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Block;
  inner_style.height = Some(Length::px(20.0));
  let inner_style = Arc::new(inner_style);

  let mut inner1 = BoxNode::new_block(inner_style.clone(), FormattingContextType::Block, vec![]);
  inner1.id = 203;
  let mut inner2 = BoxNode::new_block(inner_style, FormattingContextType::Block, vec![]);
  inner2.id = 204;

  let mut avoid_item = BoxNode::new_block(
    avoid_style,
    FormattingContextType::Block,
    vec![inner1, inner2],
  );
  avoid_item.id = 202;

  let mut flex_container = BoxNode::new_block(
    flex_style,
    FormattingContextType::Flex,
    vec![spacer.clone(), avoid_item.clone()],
  );
  flex_container.id = 200;

  let mut root =
    BoxNode::new_block(multicol_style, FormattingContextType::Block, vec![flex_container.clone()]);
  root.id = 199;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&root, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let container = find_fragment(&fragment, root.id).expect("multicol container fragment");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");
  let stride = info.column_width + info.column_gap;

  let avoid_frags = fragments_with_id(&fragment, avoid_item.id);
  assert_eq!(
    avoid_frags.len(),
    1,
    "avoid-column flex item should not split across columns (got fragments={:?})",
    avoid_frags
      .iter()
      .map(|f| (f.fragment_index, f.bounds))
      .collect::<Vec<_>>()
  );
  let avoid_frag = avoid_frags[0];
  assert!(
    (avoid_frag.bounds.height() - 40.0).abs() < 0.1,
    "avoided flex item should keep its full height (got h={})",
    avoid_frag.bounds.height()
  );

  fn first_fragment_offset(fragment: &FragmentNode, id: usize) -> Option<(f32, f32)> {
    let root_offset = (-fragment.bounds.x(), -fragment.bounds.y());
    let mut stack = vec![(fragment, root_offset)];
    while let Some((node, (ox, oy))) = stack.pop() {
      let node_offset = (ox + node.bounds.x(), oy + node.bounds.y());
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

  let (x, y) = first_fragment_offset(&fragment, avoid_item.id).expect("avoid item offset");
  assert!(
    y.abs() < 0.1,
    "expected avoided flex item to start at the top of the next column (y={y})"
  );
  assert!(
    x >= stride - 0.5,
    "expected avoided flex item to be placed in the second column (x={x}, stride={stride})"
  );
}

#[test]
fn too_tall_avoid_column_block_can_still_fragment() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.height = Some(Length::px(60.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  parent_style.column_fill = ColumnFill::Auto;
  let parent_style = Arc::new(parent_style);

  let mut avoid_style = ComputedStyle::default();
  avoid_style.break_inside = BreakInside::AvoidColumn;
  let avoid_style = Arc::new(avoid_style);

  let child_style = || -> Arc<ComputedStyle> {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(40.0));
    Arc::new(style)
  };

  let mut first = BoxNode::new_block(child_style(), FormattingContextType::Block, vec![]);
  first.id = 90;
  let mut second = BoxNode::new_block(child_style(), FormattingContextType::Block, vec![]);
  second.id = 91;
  let mut third = BoxNode::new_block(child_style(), FormattingContextType::Block, vec![]);
  third.id = 92;

  let mut avoid_block = BoxNode::new_block(
    avoid_style,
    FormattingContextType::Block,
    vec![first.clone(), second.clone(), third.clone()],
  );
  avoid_block.id = 93;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![avoid_block.clone()],
  );
  parent.id = 94;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let avoid_frags = fragments_with_id(&fragment, avoid_block.id);
  assert!(
    avoid_frags.len() > 1,
    "avoid-column block taller than the column height should still split to make progress (got fragments={:?})",
    avoid_frags
      .iter()
      .map(|f| (f.fragment_index, f.bounds))
      .collect::<Vec<_>>()
  );
  let mut indices: Vec<usize> = avoid_frags.iter().map(|f| f.fragment_index).collect();
  indices.sort_unstable();
  indices.dedup();
  assert!(
    indices.len() > 1,
    "expected avoid-column fragments to span multiple columns (indices={indices:?}, frags={:?})",
    avoid_frags
      .iter()
      .map(|f| (f.fragment_index, f.bounds))
      .collect::<Vec<_>>()
  );
}

#[test]
fn multicol_fragments_paragraph_lines_with_widows_orphans() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  let parent_style = Arc::new(parent_style);

  let mut para_style = ComputedStyle::default();
  para_style.white_space = WhiteSpace::Pre;
  para_style.widows = 2;
  para_style.orphans = 2;
  let para_style = Arc::new(para_style);

  let text = "one\ntwo\nthree\nfour\nfive\nsix".to_string();
  let text_node = BoxNode::new_text(para_style.clone(), text);
  let mut para = BoxNode::new_block(
    para_style.clone(),
    FormattingContextType::Block,
    vec![text_node],
  );
  para.id = 40;

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![para.clone()],
  );
  parent.id = 41;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let para_frags = fragments_with_id(&fragment, para.id);
  assert_eq!(para_frags.len(), 2, "paragraph should split across columns");

  let first = para_frags
    .iter()
    .find(|f| f.fragment_index == 0)
    .expect("first column fragment");
  let second = para_frags
    .iter()
    .find(|f| f.fragment_index == 1)
    .expect("second column fragment");

  assert_eq!(count_lines(first), 3);
  assert_eq!(count_lines(second), 3);
  assert!(first.bounds.x() < second.bounds.x());
}

#[test]
fn overflow_creates_additional_column_set() {
  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(200.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(0.0);
  let parent_style = Arc::new(parent_style);

  let mut children = Vec::new();
  for i in 0..4 {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(30.0));
    if i < 3 {
      style.break_after = BreakBetween::Column;
    }
    let mut child = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    child.id = 60 + i;
    children.push(child);
  }
  let third_id = children[2].id;

  let mut parent = BoxNode::new_block(parent_style, FormattingContextType::Block, children);
  parent.id = 65;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(200.0))
    .expect("layout");

  let third_frag = find_fragment(&fragment, third_id).expect("third fragment");
  let second_frag = find_fragment(&fragment, third_id - 1).expect("second fragment");
  assert!(
    (third_frag.bounds.y() - 0.0).abs() < 0.1,
    "third fragment should share the first column set block origin"
  );
  assert!(
    third_frag.bounds.x() > second_frag.bounds.x() + 0.1,
    "third fragment should overflow into an additional column in the inline direction"
  );
}

fn sample_pixel(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> (u8, u8, u8, u8) {
  let px = pixmap.pixel(x, y).expect("sample pixel");
  (px.red(), px.green(), px.blue(), px.alpha())
}

fn render_html_to_pixmap(html: &str, width: u32, height: u32) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut renderer = FastRender::with_config(config).expect("create renderer");
  renderer
    .render_html(html, width, height)
    .expect("render html")
}

fn render_multicol_overflow(overflow: &str) -> tiny_skia::Pixmap {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut renderer = FastRender::with_config(config).expect("create renderer");

  let html = format!(
    r#"<!doctype html>
      <style>
        html, body {{ margin: 0; background: rgb(255, 0, 255); }}
        .multi {{
          width: 200px;
          height: 60px;
          column-count: 2;
          column-gap: 10px;
          column-fill: auto;
          overflow: {overflow};
        }}
        .block {{ height: 40px; break-inside: avoid-column; }}
        #one {{ background: rgb(255, 0, 0); }}
        #two {{ background: rgb(0, 255, 0); }}
        #three {{ background: rgb(0, 0, 255); }}
      </style>
      <div class="multi">
        <div id="one" class="block"></div>
        <div id="two" class="block"></div>
        <div id="three" class="block"></div>
      </div>"#
  );

  renderer
    .render_html(&html, 400, 100)
    .expect("render multicol")
}

#[test]
fn overflow_hidden_clips_overflow_columns() {
  let overflow_visible = render_multicol_overflow("visible");
  assert_eq!(
    sample_pixel(&overflow_visible, 215, 20),
    (0, 0, 255, 255),
    "overflow column should be visible when overflow is not clipped"
  );

  let overflow_hidden = render_multicol_overflow("hidden");
  assert_eq!(
    sample_pixel(&overflow_hidden, 215, 20),
    (255, 0, 255, 255),
    "overflow:hidden should clip overflow columns outside the multicol container"
  );
}

#[test]
fn column_rule_is_painted_centered_in_gap() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(255, 0, 255); }
      #multi {
        width: 200px;
        height: 100px;
        column-count: 2;
        column-gap: 20px;
        column-rule: 10px solid rgb(255, 0, 0);
        column-fill: auto;
        background: rgb(255, 255, 255);
      }
      #left {
        height: 100px;
        break-after: column;
        background: rgb(0, 255, 0);
      }
      #right {
        height: 100px;
        background: rgb(0, 0, 255);
      }
    </style>
    <div id="multi">
      <div id="left"></div>
      <div id="right"></div>
    </div>
  "#;

  let pixmap = render_html_to_pixmap(html, 220, 120);

  // Rule should be centered in the 20px gap: with 10px width it leaves 5px background on each side.
  assert_eq!(sample_pixel(&pixmap, 92, 50), (255, 255, 255, 255));
  assert_eq!(sample_pixel(&pixmap, 100, 5), (255, 0, 0, 255));
  assert_eq!(sample_pixel(&pixmap, 100, 50), (255, 0, 0, 255));
  assert_eq!(sample_pixel(&pixmap, 100, 95), (255, 0, 0, 255));
  assert_eq!(sample_pixel(&pixmap, 108, 50), (255, 255, 255, 255));

  // Sanity: both columns should have visible content.
  assert_eq!(sample_pixel(&pixmap, 10, 50), (0, 255, 0, 255));
  assert_eq!(sample_pixel(&pixmap, 150, 50), (0, 0, 255, 255));
}

#[test]
fn column_rule_width_is_clamped_to_column_gap() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(255, 0, 255); }
      #multi {
        width: 200px;
        height: 100px;
        column-count: 2;
        column-gap: 20px;
        column-rule: 50px solid rgb(255, 0, 0);
        column-fill: auto;
        background: rgb(255, 255, 255);
      }
      #left {
        height: 100px;
        break-after: column;
        background: rgb(0, 255, 0);
      }
      #right {
        height: 100px;
        background: rgb(0, 0, 255);
      }
    </style>
    <div id="multi">
      <div id="left"></div>
      <div id="right"></div>
    </div>
  "#;

  let pixmap = render_html_to_pixmap(html, 220, 120);

  // Rule width must clamp to the 20px column-gap, i.e. fill the entire gap but not intrude into columns.
  assert_eq!(sample_pixel(&pixmap, 92, 50), (255, 0, 0, 255));
  assert_eq!(sample_pixel(&pixmap, 100, 50), (255, 0, 0, 255));
  assert_eq!(sample_pixel(&pixmap, 108, 50), (255, 0, 0, 255));

  // If the rule width was not clamped, it would extend into the columns.
  assert_eq!(sample_pixel(&pixmap, 80, 50), (0, 255, 0, 255));
  assert_eq!(sample_pixel(&pixmap, 120, 50), (0, 0, 255, 255));
}

#[test]
fn column_rule_is_segmented_around_spanners() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(255, 0, 255); }
      #multi {
        width: 200px;
        column-count: 2;
        column-gap: 20px;
        column-rule: 10px solid rgb(255, 0, 0);
        background: rgb(255, 255, 255);
      }
      .left {
        height: 80px;
        break-after: column;
        background: rgb(0, 255, 0);
      }
      .right {
        height: 80px;
        background: rgb(0, 0, 255);
      }
      .spanner {
        column-span: all;
        height: 40px;
        margin: 0;
      }
      .spanner > .marker {
        width: 60px;
        height: 40px;
        background: rgb(255, 255, 0);
      }
    </style>
    <div id="multi">
      <div class="left"></div>
      <div class="right"></div>
      <div class="spanner"><div class="marker"></div></div>
      <div class="left"></div>
      <div class="right"></div>
    </div>
  "#;

  let pixmap = render_html_to_pixmap(html, 220, 240);

  // First column-set: rule should be present in the gap.
  assert_eq!(sample_pixel(&pixmap, 100, 40), (255, 0, 0, 255));

  // Spanner region should not have a column rule segment. Sample x=100 outside the spanner's 60px
  // width, where we'd see the rule if it were incorrectly drawn through the spanning element.
  assert_eq!(sample_pixel(&pixmap, 10, 100), (255, 255, 0, 255));
  assert_eq!(sample_pixel(&pixmap, 100, 100), (255, 255, 255, 255));

  // Second column-set: rule should resume after the spanner.
  assert_eq!(sample_pixel(&pixmap, 100, 160), (255, 0, 0, 255));
}

#[test]
fn column_rule_is_not_painted_next_to_empty_columns() {
  // Regression for rule drawing: `column-rule` should only paint between adjacent columns that both
  // have content. If a trailing column is empty (or omitted), the rule must not be painted in that
  // gap.
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; background: rgb(255, 0, 255); }
      #multi {
        width: 340px;
        height: 100px;
        column-count: 3;
        column-gap: 20px;
        column-rule: 10px solid rgb(255, 0, 0);
        column-fill: auto;
        background: rgb(255, 255, 255);
      }
      .col { height: 100px; margin: 0; }
      #one { break-after: column; background: rgb(0, 255, 0); }
      /* Force a third, empty column by breaking after the second block. */
      #two { break-after: column; background: rgb(0, 0, 255); }
    </style>
    <div id="multi">
      <div id="one" class="col"></div>
      <div id="two" class="col"></div>
    </div>
  "#;

  let pixmap = render_html_to_pixmap(html, 360, 120);

  // Gap 1 (between col 1 + col 2) should have a rule segment.
  assert_eq!(sample_pixel(&pixmap, 110, 50), (255, 0, 0, 255));

  // Gap 2 (between col 2 + empty col 3) should not have a rule segment.
  assert_eq!(sample_pixel(&pixmap, 230, 50), (255, 255, 255, 255));

  // Sanity: content columns are present, and the trailing empty column is background.
  assert_eq!(sample_pixel(&pixmap, 10, 50), (0, 255, 0, 255));
  assert_eq!(sample_pixel(&pixmap, 150, 50), (0, 0, 255, 255));
  assert_eq!(sample_pixel(&pixmap, 290, 50), (255, 255, 255, 255));
}

fn find_first_multicol_container<'a>(fragment: &'a FragmentNode) -> Option<&'a FragmentNode> {
  if fragment.fragmentation.is_some() {
    return Some(fragment);
  }
  for child in fragment.children.iter() {
    if let Some(found) = find_first_multicol_container(child) {
      return Some(found);
    }
  }
  None
}

fn render_tree_with_artifacts(html: &str, width: u32, height: u32) -> FragmentTree {
  let toggles = RuntimeToggles::from_map(HashMap::from([(
    "FASTR_PAINT_BACKEND".to_string(),
    "display_list".to_string(),
  )]));
  let config = FastRenderConfig::new()
    .with_runtime_toggles(toggles)
    .with_paint_parallelism(PaintParallelism::disabled())
    .with_layout_parallelism(LayoutParallelism::disabled());
  let mut renderer = FastRender::with_config(config).expect("create renderer");

  let options = RenderOptions::new().with_viewport(width, height);
  let mut artifacts = RenderArtifacts::new(RenderArtifactRequest {
    fragment_tree: true,
    ..Default::default()
  });
  renderer
    .render_html_with_options_and_artifacts(html, options, &mut artifacts)
    .expect("render");

  artifacts.fragment_tree.expect("fragment tree artifact")
}

#[test]
fn column_width_without_count_generates_auto_columns() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; font-size: 16px; }
      #multi { width: 300px; column-width: 100px; }
    </style>
    <div id="multi">hello<br>world<br>more<br>lines<br>to<br>flow</div>
  "#;

  let tree = render_tree_with_artifacts(html, 800, 200);
  let container = find_first_multicol_container(&tree.root).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info");

  assert_eq!(info.column_count, 2);
  assert!((info.column_gap - 16.0).abs() < 0.1, "expected 1em gap");
  assert!(
    (info.column_width - 142.0).abs() < 0.6,
    "expected auto column width (got {})",
    info.column_width
  );
}

#[test]
fn multicol_column_gap_normal_is_1em() {
  let html = r#"<!doctype html>
    <style>html,body{margin:0;font-size:16px;}</style>
    <div id=multi style="width:300px; column-width:100px; column-gap: normal;">hello<br>world<br>more<br>lines<br>to<br>flow</div>
  "#;

  let tree = render_tree_with_artifacts(html, 800, 200);
  let container = find_first_multicol_container(&tree.root).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info");

  assert!(
    info.column_count > 1,
    "expected multi-column layout (got {})",
    info.column_count
  );
  assert!(
    (info.column_gap - 16.0).abs() < 0.1,
    "expected 1em column gap for column-gap: normal (got {})",
    info.column_gap
  );
}

#[test]
fn multicol_gap_shorthand_normal_is_1em() {
  let html = r#"<!doctype html>
    <style>html,body{margin:0;font-size:16px;}</style>
    <div id=multi style="width:300px; column-width:100px; gap: normal;">hello<br>world<br>more<br>lines<br>to<br>flow</div>
  "#;

  let tree = render_tree_with_artifacts(html, 800, 200);
  let container = find_first_multicol_container(&tree.root).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info");

  assert!(
    info.column_count > 1,
    "expected multi-column layout (got {})",
    info.column_count
  );
  assert!(
    (info.column_gap - 16.0).abs() < 0.1,
    "expected 1em column gap for gap: normal (got {})",
    info.column_gap
  );
}

#[test]
fn columns_shorthand_single_length_sets_column_width() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; font-size: 16px; }
      #multi { width: 600px; columns: 12em; }
    </style>
    <div id="multi">hello<br>world<br>more<br>lines<br>to<br>flow</div>
  "#;

  let tree = render_tree_with_artifacts(html, 800, 200);
  let container = find_first_multicol_container(&tree.root).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info");

  assert_eq!(info.column_count, 2);
  assert!((info.column_gap - 16.0).abs() < 0.1);
  assert!(
    (info.column_width - 292.0).abs() < 0.6,
    "expected auto column width (got {})",
    info.column_width
  );
  assert!(info.column_width >= 192.0);
}

#[test]
fn columns_shorthand_single_number_sets_column_count() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; font-size: 16px; }
      #multi { width: 800px; columns: 4; }
    </style>
    <div id="multi">hello<br>world<br>more<br>lines<br>to<br>flow</div>
  "#;

  let tree = render_tree_with_artifacts(html, 900, 200);
  let container = find_first_multicol_container(&tree.root).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info");

  assert_eq!(info.column_count, 4);
  assert!(
    (info.column_width - 188.0).abs() < 0.6,
    "expected computed column width (got {})",
    info.column_width
  );
}

#[test]
fn columns_shorthand_number_and_length_treats_count_as_maximum() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; font-size: 16px; }
      #multi { width: 600px; columns: 12 8em; }
    </style>
    <div id="multi">hello<br>world<br>more<br>lines<br>to<br>flow</div>
  "#;

  let tree = render_tree_with_artifacts(html, 800, 200);
  let container = find_first_multicol_container(&tree.root).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info");

  assert_eq!(info.column_count, 4);
  assert!(
    (info.column_width - 138.0).abs() < 0.6,
    "expected computed column width (got {})",
    info.column_width
  );
  assert!(info.column_width >= 128.0);
}

#[test]
fn columns_shorthand_length_and_number_in_inline_style_treats_count_as_maximum() {
  let html = r#"<!doctype html>
    <style>html,body{margin:0;font-size:16px;}</style>
    <div id=multi style="width:600px; columns: 8em 12;">hello<br>world<br>more<br>lines<br>to<br>flow</div>
  "#;

  let tree = render_tree_with_artifacts(html, 800, 200);
  let container = find_first_multicol_container(&tree.root).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info");

  assert_eq!(info.column_count, 4);
  assert!((info.column_gap - 16.0).abs() < 0.1, "expected 1em gap");
  assert!(
    (info.column_width - 138.0).abs() < 0.6,
    "expected computed column width (got {})",
    info.column_width
  );
  assert!(info.column_width >= 128.0);
}

#[test]
fn column_gap_em_resolves_against_font_size() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; font-size: 16px; }
      #multi { width: 500px; column-count: 5; column-gap: 2em; }
    </style>
    <div id="multi">hello<br>world<br>more<br>lines<br>to<br>flow</div>
  "#;

  let tree = render_tree_with_artifacts(html, 600, 200);
  let container = find_first_multicol_container(&tree.root).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info");

  assert_eq!(info.column_count, 5);
  assert!(
    (info.column_gap - 32.0).abs() < 0.2,
    "expected 2em gap (got {})",
    info.column_gap
  );
  assert!(
    (info.column_width - 74.4).abs() < 0.6,
    "expected computed column width (got {})",
    info.column_width
  );
}

#[test]
fn multicol_padding_border_box_sizing_uses_content_box_geometry() {
  let html = r#"<!doctype html>
    <style>
      html, body { margin: 0; font-size: 16px; }
      #multi {
        box-sizing: border-box;
        width: 300px;
        padding: 20px;
        border: 5px solid black;
        column-count: 2;
        column-gap: 10px;
      }
      #multi p { margin: 0; }
    </style>
    <div id=multi>
      <p>line1</p><p>line2</p><p>line3</p><p>line4</p><p>line5</p>
    </div>
  "#;

  let tree = render_tree_with_artifacts(html, 800, 200);
  let container = find_first_multicol_container(&tree.root).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info");

  let expected_content_width = 300.0 - 2.0 * (20.0 + 5.0);
  let expected_column_width = (expected_content_width - 10.0) / 2.0;

  assert_eq!(info.column_count, 2);
  assert!(
    (info.column_gap - 10.0).abs() < 0.1,
    "expected 10px gap (got {})",
    info.column_gap
  );
  assert!(
    (info.column_width - expected_column_width).abs() < 0.1,
    "expected column width from content box (got {})",
    info.column_width
  );

  let reconstructed_content_width = info.column_width * info.column_count as f32
    + info.column_gap * (info.column_count.saturating_sub(1) as f32);
  assert!(
    (reconstructed_content_width - expected_content_width).abs() < 0.2,
    "expected column geometry to be based on content width {} (got {})",
    expected_content_width,
    reconstructed_content_width
  );

  let mut line_positions = Vec::new();
  collect_line_positions(container, (0.0, 0.0), &mut line_positions);
  assert!(
    !line_positions.is_empty(),
    "expected line fragments inside multicol container"
  );

  let expected_first_column_x = container.bounds.x() + 25.0;
  let expected_second_column_x = expected_first_column_x + expected_column_width + info.column_gap;
  let tol = 0.6;

  let min_x = line_positions
    .iter()
    .map(|(x, _)| *x)
    .fold(f32::INFINITY, f32::min);
  assert!(
    (min_x - expected_first_column_x).abs() < tol,
    "columns should start inside padding/border (min x={}, expected around {})",
    min_x,
    expected_first_column_x
  );

  assert!(
    line_positions
      .iter()
      .any(|(x, _)| (*x - expected_first_column_x).abs() < tol),
    "expected some line boxes in the first column (x≈{}, got {:?})",
    expected_first_column_x,
    line_positions
  );
  assert!(
    line_positions
      .iter()
      .any(|(x, _)| (*x - expected_second_column_x).abs() < tol),
    "expected some line boxes in the second column (x≈{}, got {:?})",
    expected_second_column_x,
    line_positions
  );
}

#[test]
fn column_rule_fragments_are_generated_clamped_and_centered() {
  let color = Rgba::new(10, 20, 30, 1.0);

  let layout_with_rule_width = |rule_width: f32| -> (FragmentNode, usize) {
    let mut parent_style = ComputedStyle::default();
    parent_style.width = Some(Length::px(300.0));
    parent_style.column_count = Some(2);
    parent_style.column_gap = Length::px(20.0);
    parent_style.column_rule_style = BorderStyle::Solid;
    parent_style.column_rule_width = Length::px(rule_width);
    parent_style.column_rule_color = Some(color);
    let parent_style = Arc::new(parent_style);

    let mut first_style = ComputedStyle::default();
    first_style.height = Some(Length::px(10.0));
    first_style.break_after = BreakBetween::Column;
    let mut first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
    first.id = 1;

    let mut second_style = ComputedStyle::default();
    second_style.height = Some(Length::px(10.0));
    let mut second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
    second.id = 2;

    let mut parent =
      BoxNode::new_block(parent_style, FormattingContextType::Block, vec![first, second]);
    parent.id = 100;

    let fc = BlockFormattingContext::new();
    let fragment = fc
      .layout(&parent, &LayoutConstraints::definite_width(300.0))
      .expect("layout");
    (fragment, parent.id)
  };

  // When the declared rule width is larger than the column gap, it should clamp to the gap.
  let (fragment, parent_id) = layout_with_rule_width(50.0);
  let container = find_fragment(&fragment, parent_id).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");
  let rule = find_rule_fragment(container, color).expect("column rule fragment");
  assert_eq!(count_rule_fragments(container, color), 1);
  assert!(
    (rule.bounds.width() - info.column_gap).abs() < 0.01,
    "rule width should clamp to the gap (got w={}, gap={})",
    rule.bounds.width(),
    info.column_gap
  );
  let expected_x = info.column_width;
  assert!(
    (rule.bounds.x() - expected_x).abs() < 0.01,
    "clamped rule should start at the gap start (got x={}, expected={})",
    rule.bounds.x(),
    expected_x
  );

  // When the rule width is smaller than the gap, it should be centered between columns.
  let (fragment, parent_id) = layout_with_rule_width(8.0);
  let container = find_fragment(&fragment, parent_id).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");
  let rule = find_rule_fragment(container, color).expect("column rule fragment");
  assert_eq!(count_rule_fragments(container, color), 1);
  assert!(
    (rule.bounds.width() - 8.0).abs() < 0.01,
    "expected unclamped rule width (got w={})",
    rule.bounds.width()
  );
  let expected_x = info.column_width + (info.column_gap - 8.0) * 0.5;
  assert!(
    (rule.bounds.x() - expected_x).abs() < 0.01,
    "rule should be centered in the gap (got x={}, expected={})",
    rule.bounds.x(),
    expected_x
  );
}

#[test]
fn column_rule_is_not_generated_between_empty_columns() {
  let color = Rgba::new(42, 10, 200, 1.0);

  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(300.0));
  parent_style.column_count = Some(3);
  parent_style.column_gap = Length::px(20.0);
  parent_style.column_rule_style = BorderStyle::Solid;
  parent_style.column_rule_width = Length::px(6.0);
  parent_style.column_rule_color = Some(color);
  let parent_style = Arc::new(parent_style);

  // Force content into exactly two columns, leaving the third one empty.
  let mut first_style = ComputedStyle::default();
  first_style.height = Some(Length::px(10.0));
  first_style.break_after = BreakBetween::Column;
  let mut first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  first.id = 11;

  let mut second_style = ComputedStyle::default();
  second_style.height = Some(Length::px(10.0));
  let mut second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  second.id = 12;

  let mut parent =
    BoxNode::new_block(parent_style, FormattingContextType::Block, vec![first, second]);
  parent.id = 110;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(300.0))
    .expect("layout");

  let container = find_fragment(&fragment, parent.id).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");
  assert_eq!(info.column_count, 3);

  let rules = collect_rule_fragments(container, color);
  assert_eq!(
    rules.len(),
    1,
    "expected a single rule fragment (only between the two non-empty columns)"
  );
  let rule = rules[0];
  let expected_x = info.column_width + (info.column_gap - 6.0) * 0.5;
  assert!(
    (rule.bounds.x() - expected_x).abs() < 0.01,
    "expected rule between the first and second columns (got x={}, expected={})",
    rule.bounds.x(),
    expected_x
  );
}

#[test]
fn column_rule_fragments_are_split_per_column_set_around_spanner() {
  let color = Rgba::new(150, 50, 0, 1.0);

  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(300.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(20.0);
  parent_style.column_rule_style = BorderStyle::Solid;
  parent_style.column_rule_width = Length::px(8.0);
  parent_style.column_rule_color = Some(color);
  let parent_style = Arc::new(parent_style);

  let block = |id: usize, break_after: bool| -> BoxNode {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(10.0));
    if break_after {
      style.break_after = BreakBetween::Column;
    }
    let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    node.id = id;
    node
  };

  let before_left = block(1, true);
  let before_right = block(2, false);

  let mut span_style = ComputedStyle::default();
  span_style.height = Some(Length::px(15.0));
  span_style.column_span = ColumnSpan::All;
  let mut span = BoxNode::new_block(Arc::new(span_style), FormattingContextType::Block, vec![]);
  span.id = 3;

  let after_left = block(4, true);
  let after_right = block(5, false);

  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![
      before_left,
      before_right,
      span.clone(),
      after_left,
      after_right,
    ],
  );
  parent.id = 200;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(300.0))
    .expect("layout");

  let container = find_fragment(&fragment, parent.id).expect("multicol container");
  let span_frag = find_fragment(&fragment, span.id).expect("spanner fragment");

  let mut rules = collect_rule_fragments(container, color);
  rules.sort_by(|a, b| {
    a.bounds
      .y()
      .partial_cmp(&b.bounds.y())
      .unwrap_or(std::cmp::Ordering::Equal)
  });
  assert_eq!(
    rules.len(),
    2,
    "expected one rule fragment per column set (got {:#?})",
    rules.iter().map(|r| r.bounds).collect::<Vec<_>>()
  );

  for rule in &rules {
    let overlaps_spanner = rule.bounds.y() < span_frag.bounds.max_y() - 0.01
      && rule.bounds.max_y() > span_frag.bounds.y() + 0.01;
    assert!(
      !overlaps_spanner,
      "rule fragment should not overlap the spanner (rule={:?}, spanner={:?})",
      rule.bounds,
      span_frag.bounds
    );
  }

  assert!(
    rules[0].bounds.max_y() <= span_frag.bounds.y() + 0.05,
    "first set rule should end at the spanner start (rule={:?}, spanner={:?})",
    rules[0].bounds,
    span_frag.bounds
  );
  assert!(
    rules[1].bounds.y() >= span_frag.bounds.max_y() - 0.05,
    "second set rule should start after the spanner (rule={:?}, spanner={:?})",
    rules[1].bounds,
    span_frag.bounds
  );
}

#[test]
fn column_rule_fragments_are_split_per_column_set_in_paged_multicol() {
  // In a paged fragmentation context (`fragmentainer_block_size_hint`), additional columns overflow
  // into *new column sets* stacked in the block direction. Column rules must be segmented per set
  // rather than spanning the entire multi-column flow.
  let page_size = 50.0;
  let _hint_guard = set_fragmentainer_block_size_hint(Some(page_size));
  let _offset_guard = set_fragmentainer_block_offset_hint(0.0);

  let color = Rgba::new(200, 0, 0, 1.0);

  let mut parent_style = ComputedStyle::default();
  parent_style.width = Some(Length::px(300.0));
  parent_style.column_count = Some(2);
  parent_style.column_gap = Length::px(20.0);
  parent_style.column_fill = ColumnFill::Auto;
  parent_style.column_rule_style = BorderStyle::Solid;
  parent_style.column_rule_width = Length::px(8.0);
  parent_style.column_rule_color = Some(color);
  let parent_style = Arc::new(parent_style);

  let block = |id: usize, break_after: bool| -> BoxNode {
    let mut style = ComputedStyle::default();
    style.height = Some(Length::px(10.0));
    if break_after {
      style.break_after = BreakBetween::Column;
    }
    let mut node = BoxNode::new_block(Arc::new(style), FormattingContextType::Block, vec![]);
    node.id = id;
    node
  };

  // Force four columns worth of content. With `column-count:2` this requires two column sets.
  let mut parent = BoxNode::new_block(
    parent_style,
    FormattingContextType::Block,
    vec![block(1, true), block(2, true), block(3, true), block(4, false)],
  );
  parent.id = 250;

  let fc = BlockFormattingContext::new();
  let fragment = fc
    .layout(&parent, &LayoutConstraints::definite_width(300.0))
    .expect("layout");

  let container = find_fragment(&fragment, parent.id).expect("multicol container");
  let info = container
    .fragmentation
    .as_ref()
    .expect("fragmentation info for multicol container");

  let mut rules = collect_rule_fragments(container, color);
  rules.sort_by(|a, b| {
    a.bounds
      .y()
      .partial_cmp(&b.bounds.y())
      .unwrap_or(std::cmp::Ordering::Equal)
  });
  assert_eq!(
    rules.len(),
    2,
    "expected one rule per column set in paged multicol (got {:#?})",
    rules.iter().map(|r| r.bounds).collect::<Vec<_>>()
  );

  let expected_x = info.column_width + (info.column_gap - 8.0) * 0.5;
  for rule in &rules {
    assert!(
      (rule.bounds.x() - expected_x).abs() < 0.05,
      "rule should be centered in the gap (got x={}, expected={})",
      rule.bounds.x(),
      expected_x
    );
    assert!(
      (rule.bounds.width() - 8.0).abs() < 0.05,
      "expected rule width to remain 8px (got w={})",
      rule.bounds.width()
    );
    assert!(
      (rule.bounds.height() - page_size).abs() < 0.05,
      "expected rule to extend the full fragmentainer height in paged multicol (got h={}, expected={})",
      rule.bounds.height(),
      page_size
    );
  }

  assert!(
    (rules[0].bounds.y() - 0.0).abs() < 0.05,
    "first set rule should start at the top of the flow (got y={})",
    rules[0].bounds.y()
  );
  assert!(
    (rules[1].bounds.y() - page_size).abs() < 0.05,
    "second set rule should be offset by one fragmentainer (got y={}, expected={})",
    rules[1].bounds.y(),
    page_size
  );
  assert!(
    rules[0].bounds.max_y() <= page_size + 0.05,
    "first set rule should not extend past the first fragmentainer (rule={:?})",
    rules[0].bounds
  );
  assert!(
    rules[1].bounds.y() >= page_size - 0.05,
    "second set rule should not start before the second fragmentainer (rule={:?})",
    rules[1].bounds
  );
}
