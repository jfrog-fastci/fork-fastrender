use fastrender::debug::runtime::RuntimeToggles;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::block::BlockFormattingContext;
use fastrender::layout::engine::LayoutParallelism;
use fastrender::paint::display_list_builder::DisplayListBuilder;
use fastrender::paint::display_list_renderer::PaintParallelism;
use fastrender::style::color::Rgba;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::BorderStyle;
use fastrender::style::types::BreakBetween;
use fastrender::style::types::BreakInside;
use fastrender::style::types::ColumnFill;
use fastrender::style::types::ColumnSpan;
use fastrender::style::types::WhiteSpace;
use fastrender::style::types::WritingMode;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::{FragmentContent, FragmentNode, FragmentTree};
use fastrender::{FastRender, FastRenderConfig, RenderArtifactRequest, RenderArtifacts, RenderOptions};
use fastrender::FormattingContext;
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

  let root = BoxNode::new_block(parent_style, FormattingContextType::Block, vec![first, second]);

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
    if let fastrender::DisplayItem::Border(border) = item {
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

  renderer.render_html(&html, 400, 100).expect("render multicol")
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
  let info = container.fragmentation.as_ref().expect("fragmentation info");

  assert_eq!(info.column_count, 2);
  assert!((info.column_gap - 16.0).abs() < 0.1, "expected 1em gap");
  assert!(
    (info.column_width - 142.0).abs() < 0.6,
    "expected auto column width (got {})",
    info.column_width
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
  let info = container.fragmentation.as_ref().expect("fragmentation info");

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
  let info = container.fragmentation.as_ref().expect("fragmentation info");

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
  let info = container.fragmentation.as_ref().expect("fragmentation info");

  assert_eq!(info.column_count, 4);
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
  let info = container.fragmentation.as_ref().expect("fragmentation info");

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
