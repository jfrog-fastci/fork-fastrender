use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::flex::FlexFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::position::Position;
use fastrender::style::types::AlignItems;
use fastrender::style::types::FlexDirection;
use fastrender::style::types::LineHeight;
use fastrender::style::types::WritingMode;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use fastrender::tree::fragment_tree::FragmentContent;
use fastrender::tree::fragment_tree::FragmentNode;
use std::sync::Arc;

fn first_baseline_offset(fragment: &FragmentNode) -> Option<f32> {
  if let Some(style) = fragment.style.as_deref() {
    if style.running_position.is_some() || matches!(style.position, Position::Absolute | Position::Fixed) {
      return None;
    }
  }

  if let Some(baseline) = fragment.baseline {
    return Some(baseline);
  }

  match &fragment.content {
    FragmentContent::Line { baseline } => Some(*baseline),
    FragmentContent::Text { baseline_offset, .. } => Some(*baseline_offset),
    FragmentContent::Replaced { .. } => Some(fragment.bounds.height()),
    _ => {
      for child in fragment.children.iter() {
        if let Some(b) = first_baseline_offset(child) {
          return Some(child.bounds.y() + b);
        }
      }
      None
    }
  }
}

fn block_axis_is_horizontal(wm: WritingMode) -> bool {
  matches!(
    wm,
    WritingMode::VerticalRl | WritingMode::VerticalLr | WritingMode::SidewaysRl | WritingMode::SidewaysLr
  )
}

fn block_axis_positive(wm: WritingMode) -> bool {
  !matches!(wm, WritingMode::VerticalRl | WritingMode::SidewaysRl)
}

fn find_first_baseline_offset_x(fragment: &FragmentNode, block_positive: bool) -> Option<f32> {
  if let Some(style) = fragment.style.as_deref() {
    if style.running_position.is_some() || matches!(style.position, Position::Absolute | Position::Fixed) {
      return None;
    }
  }

  let resolve_from_block_start = |offset: f32, extent: f32| -> f32 {
    if block_positive {
      offset
    } else if extent.is_finite() && extent > 0.0 {
      (extent - offset).max(0.0)
    } else {
      offset
    }
  };

  let extent = fragment.bounds.width();
  if let Some(baseline) = fragment.baseline {
    return Some(resolve_from_block_start(baseline, extent));
  }
  match &fragment.content {
    FragmentContent::Line { baseline } => return Some(resolve_from_block_start(*baseline, extent)),
    FragmentContent::Text { baseline_offset, .. } => return Some(resolve_from_block_start(*baseline_offset, extent)),
    FragmentContent::Replaced { .. } => return Some(resolve_from_block_start(extent, extent)),
    _ => {}
  }

  for child in fragment.children.iter() {
    if let Some(baseline) = find_first_baseline_offset_x(child, block_positive) {
      return Some(child.bounds.x() + baseline);
    }
  }

  None
}

fn baseline_offset_x_with_fallback(fragment: &FragmentNode, writing_mode: WritingMode) -> f32 {
  let width = fragment.bounds.width().max(0.0);
  if !block_axis_is_horizontal(writing_mode) {
    return width;
  }
  find_first_baseline_offset_x(fragment, block_axis_positive(writing_mode))
    .unwrap_or(width)
    .clamp(0.0, width)
}

#[test]
fn flex_align_items_baseline_influences_cross_size() {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.width = Some(Length::px(200.0));
  container_style.align_items = AlignItems::Baseline;

  // Child A has no in-flow baseline (empty block). Its baseline should fall back to the bottom
  // edge of its margin box (i.e., its height).
  let mut child_a_style = ComputedStyle::default();
  child_a_style.display = Display::Block;
  child_a_style.width = Some(Length::px(10.0));
  child_a_style.height = Some(Length::px(100.0));
  let child_a = BoxNode::new_block(Arc::new(child_a_style), FormattingContextType::Block, vec![]);

  // Child B has a baseline near the top (text at y=0) but a large fixed height, so baseline
  // alignment should push it down and increase the line's cross size beyond 100px.
  let mut text_style = ComputedStyle::default();
  text_style.display = Display::Inline;
  text_style.font_size = 20.0;
  let text = BoxNode::new_text(Arc::new(text_style), "Hello".to_string());

  let mut child_b_style = ComputedStyle::default();
  child_b_style.display = Display::Block;
  child_b_style.width = Some(Length::px(10.0));
  child_b_style.height = Some(Length::px(100.0));
  let child_b =
    BoxNode::new_block(Arc::new(child_b_style), FormattingContextType::Block, vec![text]);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_a, child_b],
  );

  let fc = FlexFormattingContext::new();
  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(200.0), AvailableSpace::Indefinite),
    )
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 2);
  let child_a_frag = &fragment.children[0];
  let child_b_frag = &fragment.children[1];

  let baseline_a = first_baseline_offset(child_a_frag).unwrap_or(child_a_frag.bounds.height());
  let baseline_b = first_baseline_offset(child_b_frag).unwrap_or(child_b_frag.bounds.height());
  let max_baseline = baseline_a.max(baseline_b);

  let expected_cross = [
    max_baseline - baseline_a + child_a_frag.bounds.height(),
    max_baseline - baseline_b + child_b_frag.bounds.height(),
  ]
  .into_iter()
  .fold(0.0, f32::max);

  let container_cross = fragment.bounds.height();
  let eps = 0.75;
  assert!(
    (container_cross - expected_cross).abs() < eps,
    "expected baseline alignment to increase the flex line cross size (expected≈{expected_cross:.2}, got {container_cross:.2})"
  );

  let baseline_pos_a = child_a_frag.bounds.y() + baseline_a;
  let baseline_pos_b = child_b_frag.bounds.y() + baseline_b;
  assert!(
    (baseline_pos_a - baseline_pos_b).abs() < eps,
    "baselines should align (child_a={baseline_pos_a:.2}, child_b={baseline_pos_b:.2})"
  );
}

fn run_flex_align_items_baseline_aligns_x_baselines_in_vertical_writing_mode(writing_mode: WritingMode) {
  let fc = FlexFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Flex;
  container_style.writing_mode = writing_mode;
  container_style.flex_direction = FlexDirection::Row;
  container_style.align_items = AlignItems::Baseline;
  container_style.height = Some(Length::px(200.0));

  let line_height = 20.0;
  let make_item = |font_size: f32| {
    let mut wrapper_style = ComputedStyle::default();
    wrapper_style.display = Display::Block;
    wrapper_style.writing_mode = writing_mode;
    wrapper_style.width = Some(Length::px(60.0));
    wrapper_style.font_size = font_size;
    wrapper_style.line_height = LineHeight::Length(Length::px(line_height));
    let wrapper_style = Arc::new(wrapper_style);

    let mut text_style = ComputedStyle::default();
    text_style.display = Display::Inline;
    text_style.writing_mode = writing_mode;
    text_style.font_size = font_size;
    text_style.line_height = LineHeight::Length(Length::px(line_height));
    let text_child = BoxNode::new_text(Arc::new(text_style), "A".to_string());

    BoxNode::new_block(wrapper_style, FormattingContextType::Inline, vec![text_child])
  };

  let child_large = make_item(20.0);
  let child_small = make_item(5.0);

  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Flex,
    vec![child_large, child_small],
  );

  let fragment = fc
    .layout(
      &container,
      &LayoutConstraints::new(AvailableSpace::Definite(300.0), AvailableSpace::Definite(200.0)),
    )
    .expect("layout should succeed");

  assert_eq!(fragment.children.len(), 2);
  let a = &fragment.children[0];
  let b = &fragment.children[1];

  let baseline_a = baseline_offset_x_with_fallback(a, writing_mode);
  let baseline_b = baseline_offset_x_with_fallback(b, writing_mode);
  assert!(
    (baseline_a - baseline_b).abs() > 1.0,
    "expected baseline offsets to differ (a={baseline_a:.2}, b={baseline_b:.2})"
  );

  let baseline_pos_a = a.bounds.x() + baseline_a;
  let baseline_pos_b = b.bounds.x() + baseline_b;
  let eps = 0.75;
  assert!(
    (baseline_pos_a - baseline_pos_b).abs() < eps,
    "expected x baselines to align (a={baseline_pos_a:.2}, b={baseline_pos_b:.2})"
  );

  for (label, item) in [("a", a), ("b", b)] {
    let right = item.bounds.x() + item.bounds.width();
    assert!(
      item.bounds.x() >= -0.5 && right <= fragment.bounds.width() + 0.5,
      "expected {label} to fit within container cross size (x={:.2}, right={:.2}, container_w={:.2})",
      item.bounds.x(),
      right,
      fragment.bounds.width()
    );
  }
}

#[test]
fn flex_align_items_baseline_aligns_x_baselines_in_vertical_writing_mode_vertical_lr() {
  run_flex_align_items_baseline_aligns_x_baselines_in_vertical_writing_mode(WritingMode::VerticalLr);
}

#[test]
fn flex_align_items_baseline_aligns_x_baselines_in_vertical_writing_mode_vertical_rl() {
  run_flex_align_items_baseline_aligns_x_baselines_in_vertical_writing_mode(WritingMode::VerticalRl);
}
