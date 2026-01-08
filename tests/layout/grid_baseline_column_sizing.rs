use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignItems;
use fastrender::style::types::GridTrack;
use fastrender::style::types::LineHeight;
use fastrender::style::types::WritingMode;
use fastrender::style::values::Length;
use fastrender::AvailableSpace;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FragmentContent;
use fastrender::FragmentNode;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn assert_approx(actual: f32, expected: f32, label: &str) {
  assert!(
    (actual - expected).abs() <= 0.5,
    "{label}: got {actual:.2} expected {expected:.2}"
  );
}

fn block_axis_is_horizontal(wm: WritingMode) -> bool {
  matches!(
    wm,
    WritingMode::VerticalRl
      | WritingMode::VerticalLr
      | WritingMode::SidewaysRl
      | WritingMode::SidewaysLr
  )
}

fn block_axis_positive(wm: WritingMode) -> bool {
  !matches!(wm, WritingMode::VerticalRl | WritingMode::SidewaysRl)
}

fn find_first_baseline_offset_x(fragment: &FragmentNode, block_positive: bool) -> Option<f32> {
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
    FragmentContent::Text {
      baseline_offset, ..
    } => return Some(resolve_from_block_start(*baseline_offset, extent)),
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
  if !block_axis_is_horizontal(writing_mode) {
    return fragment.bounds.width();
  }
  find_first_baseline_offset_x(fragment, block_axis_positive(writing_mode))
    .unwrap_or_else(|| fragment.bounds.width())
}

fn expected_baseline_track_size_x(items: [&FragmentNode; 2], writing_mode: WritingMode) -> f32 {
  let mut max_baseline: f32 = 0.0;
  let mut max_descent: f32 = 0.0;
  for item in items {
    let width = item.bounds.width().max(0.0);
    let baseline = baseline_offset_x_with_fallback(item, writing_mode).min(width);
    max_baseline = max_baseline.max(baseline);
    max_descent = max_descent.max((width - baseline).max(0.0));
  }
  max_baseline + max_descent
}

fn make_text_item(
  id: usize,
  font_size: f32,
  line_height_px: f32,
  writing_mode: WritingMode,
  width: f32,
  row: i32,
) -> BoxNode {
  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::Block;
  wrapper_style.font_size = font_size;
  wrapper_style.line_height = LineHeight::Length(Length::px(line_height_px));
  wrapper_style.writing_mode = writing_mode;
  wrapper_style.width = Some(Length::px(width));
  wrapper_style.justify_self = Some(AlignItems::Baseline);
  wrapper_style.grid_column_start = 1;
  wrapper_style.grid_column_end = 2;
  wrapper_style.grid_row_start = row;
  wrapper_style.grid_row_end = row + 1;
  let wrapper_style = Arc::new(wrapper_style);

  let mut text_style = ComputedStyle::default();
  text_style.font_size = font_size;
  text_style.line_height = LineHeight::Length(Length::px(line_height_px));
  text_style.writing_mode = writing_mode;
  let text_style = Arc::new(text_style);

  let text_child = BoxNode::new_text(text_style, "A".to_string());
  let mut item = BoxNode::new_block(
    wrapper_style,
    FormattingContextType::Inline,
    vec![text_child],
  );
  item.id = id;
  item
}

#[test]
fn grid_baseline_aligned_items_increase_auto_column_size() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.grid_template_columns = vec![GridTrack::Auto];
  container_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  container_style.justify_items = AlignItems::Baseline;
  let container_style = Arc::new(container_style);

  let writing_mode = WritingMode::VerticalLr;
  let width = 60.0;
  // Keep line-height fixed so both items have comparable widths, but vary font-size so their
  // horizontal baselines differ (forcing the auto column to grow beyond either item's own width).
  let line_height = 20.0;
  let item_large = make_text_item(21, 20.0, line_height, writing_mode, width, 1);
  let item_small = make_text_item(22, 5.0, line_height, writing_mode, width, 2);

  let grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![item_large, item_small],
  );

  let fragment = fc
    .layout(
      &grid,
      &LayoutConstraints::new(AvailableSpace::Indefinite, AvailableSpace::Definite(200.0)),
    )
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2, "grid should have two item fragments");
  let a = &fragment.children[0];
  let b = &fragment.children[1];

  let expected_width = expected_baseline_track_size_x([a, b], writing_mode);
  assert_approx(fragment.bounds.width(), expected_width, "grid column width");

  let max_item_width = a.bounds.width().max(b.bounds.width());
  assert!(
    fragment.bounds.width() > max_item_width + 0.5,
    "expected baseline alignment to increase column width beyond either item (col={:.2}, max_item={:.2})",
    fragment.bounds.width(),
    max_item_width
  );

  let baseline_a = a.bounds.x() + baseline_offset_x_with_fallback(a, writing_mode);
  let baseline_b = b.bounds.x() + baseline_offset_x_with_fallback(b, writing_mode);
  assert!(
    (baseline_a - baseline_b).abs() <= 0.5,
    "expected baselines to align (a={baseline_a:.2}, b={baseline_b:.2})"
  );

  for (label, item) in [("a", a), ("b", b)] {
    let right = item.bounds.x() + item.bounds.width();
    assert!(
      item.bounds.x() >= -0.5 && right <= fragment.bounds.width() + 0.5,
      "expected {label} to fit in column (x={:.2}, right={:.2}, col_width={:.2})",
      item.bounds.x(),
      right,
      fragment.bounds.width()
    );
  }
}

#[test]
fn grid_baseline_aligned_items_increase_auto_column_size_vertical_rl() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.grid_template_columns = vec![GridTrack::Auto];
  container_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  container_style.justify_items = AlignItems::Baseline;
  let container_style = Arc::new(container_style);

  let writing_mode = WritingMode::VerticalRl;
  let width = 60.0;
  let line_height = 20.0;
  let item_large = make_text_item(31, 20.0, line_height, writing_mode, width, 1);
  let item_small = make_text_item(32, 5.0, line_height, writing_mode, width, 2);

  let grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![item_large, item_small],
  );

  let fragment = fc
    .layout(
      &grid,
      &LayoutConstraints::new(AvailableSpace::Indefinite, AvailableSpace::Definite(200.0)),
    )
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2, "grid should have two item fragments");
  let a = &fragment.children[0];
  let b = &fragment.children[1];

  let expected_width = expected_baseline_track_size_x([a, b], writing_mode);
  assert_approx(fragment.bounds.width(), expected_width, "grid column width");

  let max_item_width = a.bounds.width().max(b.bounds.width());
  assert!(
    fragment.bounds.width() > max_item_width + 0.5,
    "expected baseline alignment to increase column width beyond either item (col={:.2}, max_item={:.2})",
    fragment.bounds.width(),
    max_item_width
  );

  let baseline_a = a.bounds.x() + baseline_offset_x_with_fallback(a, writing_mode);
  let baseline_b = b.bounds.x() + baseline_offset_x_with_fallback(b, writing_mode);
  assert!(
    (baseline_a - baseline_b).abs() <= 0.5,
    "expected baselines to align (a={baseline_a:.2}, b={baseline_b:.2})"
  );

  for (label, item) in [("a", a), ("b", b)] {
    let right = item.bounds.x() + item.bounds.width();
    assert!(
      item.bounds.x() >= -0.5 && right <= fragment.bounds.width() + 0.5,
      "expected {label} to fit in column (x={:.2}, right={:.2}, col_width={:.2})",
      item.bounds.x(),
      right,
      fragment.bounds.width()
    );
  }
}

#[test]
fn baseline_aligned_items_in_subgrid_increase_parent_auto_column_size() {
  let fc = GridFormattingContext::new();

  let mut parent_style = ComputedStyle::default();
  parent_style.display = Display::Grid;
  parent_style.grid_template_columns = vec![GridTrack::Auto];
  parent_style.grid_template_rows = vec![GridTrack::Auto];
  let parent_style = Arc::new(parent_style);

  let mut subgrid_style = ComputedStyle::default();
  subgrid_style.display = Display::Grid;
  subgrid_style.grid_column_subgrid = true;
  subgrid_style.grid_column_start = 1;
  subgrid_style.grid_column_end = 2;
  subgrid_style.grid_row_start = 1;
  subgrid_style.grid_row_end = 2;
  // Local rows so we can place two items in different rows.
  subgrid_style.grid_template_rows = vec![GridTrack::Auto, GridTrack::Auto];
  // Baseline alignment happens within the subgrid (and contributes to the inherited column).
  subgrid_style.justify_items = AlignItems::Baseline;
  let subgrid_style = Arc::new(subgrid_style);

  let writing_mode = WritingMode::VerticalLr;
  let width = 60.0;
  let line_height = 20.0;
  let item_large = make_text_item(41, 20.0, line_height, writing_mode, width, 1);
  let item_small = make_text_item(42, 5.0, line_height, writing_mode, width, 2);

  let mut subgrid =
    BoxNode::new_block(subgrid_style, FormattingContextType::Grid, vec![item_large, item_small]);
  subgrid.id = 40;

  let grid = BoxNode::new_block(parent_style, FormattingContextType::Grid, vec![subgrid]);

  let fragment = fc
    .layout(
      &grid,
      &LayoutConstraints::new(AvailableSpace::Indefinite, AvailableSpace::Definite(200.0)),
    )
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1, "parent grid should have one child (subgrid)");
  let subgrid_fragment = &fragment.children[0];
  assert_eq!(
    subgrid_fragment.children.len(),
    2,
    "subgrid should have two item fragments"
  );
  let a = &subgrid_fragment.children[0];
  let b = &subgrid_fragment.children[1];

  let expected_width = expected_baseline_track_size_x([a, b], writing_mode);
  assert_approx(fragment.bounds.width(), expected_width, "parent column width");
  assert_approx(subgrid_fragment.bounds.width(), expected_width, "subgrid width");

  let max_item_width = a.bounds.width().max(b.bounds.width());
  assert!(
    fragment.bounds.width() > max_item_width + 0.5,
    "expected baseline alignment to increase column width beyond either item (col={:.2}, max_item={:.2})",
    fragment.bounds.width(),
    max_item_width
  );

  let baseline_a = a.bounds.x() + baseline_offset_x_with_fallback(a, writing_mode);
  let baseline_b = b.bounds.x() + baseline_offset_x_with_fallback(b, writing_mode);
  assert!(
    (baseline_a - baseline_b).abs() <= 0.5,
    "expected baselines to align within subgrid (a={baseline_a:.2}, b={baseline_b:.2})"
  );
}
