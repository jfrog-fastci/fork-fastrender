use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::AlignItems;
use fastrender::style::types::GridTrack;
use fastrender::style::types::LineHeight;
use fastrender::style::values::Length;
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

fn find_first_baseline_offset(fragment: &FragmentNode) -> Option<f32> {
  if let Some(baseline) = fragment.baseline {
    return Some(baseline);
  }
  match &fragment.content {
    FragmentContent::Line { baseline } => return Some(*baseline),
    FragmentContent::Text {
      baseline_offset, ..
    } => return Some(*baseline_offset),
    _ => {}
  }

  for child in fragment.children.iter() {
    if let Some(baseline) = find_first_baseline_offset(child) {
      return Some(child.bounds.y() + baseline);
    }
  }

  None
}

fn baseline_offset_with_fallback(fragment: &FragmentNode) -> f32 {
  find_first_baseline_offset(fragment).unwrap_or_else(|| fragment.bounds.height())
}

fn expected_baseline_track_size(items: [&FragmentNode; 2]) -> f32 {
  let mut max_baseline: f32 = 0.0;
  let mut max_descent: f32 = 0.0;
  for item in items {
    let height = item.bounds.height().max(0.0);
    let baseline = baseline_offset_with_fallback(item).min(height);
    max_baseline = max_baseline.max(baseline);
    max_descent = max_descent.max((height - baseline).max(0.0));
  }
  max_baseline + max_descent
}

fn make_text_item(id: usize, font_size: f32, line_height_px: f32, column: i32) -> BoxNode {
  let mut wrapper_style = ComputedStyle::default();
  wrapper_style.display = Display::Block;
  wrapper_style.font_size = font_size;
  wrapper_style.line_height = LineHeight::Length(Length::px(line_height_px));
  wrapper_style.align_self = Some(AlignItems::Baseline);
  wrapper_style.grid_column_start = column;
  wrapper_style.grid_column_end = column + 1;
  wrapper_style.grid_row_start = 1;
  wrapper_style.grid_row_end = 2;
  let wrapper_style = Arc::new(wrapper_style);

  let mut text_style = ComputedStyle::default();
  text_style.font_size = font_size;
  text_style.line_height = LineHeight::Length(Length::px(line_height_px));
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
fn grid_baseline_aligned_items_increase_auto_row_size() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(200.0));
  container_style.grid_template_columns = vec![GridTrack::Auto, GridTrack::Auto];
  container_style.grid_template_rows = vec![GridTrack::Auto];
  let container_style = Arc::new(container_style);

  // Keep line-height fixed so both items have comparable heights, but vary font-size so their
  // baselines differ (forcing the auto row to grow beyond either item's own height).
  let line_height = 20.0;
  let item_large = make_text_item(11, 20.0, line_height, 1);
  let item_small = make_text_item(12, 5.0, line_height, 2);

  let grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![item_large, item_small],
  );

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2, "grid should have two item fragments");
  let a = &fragment.children[0];
  let b = &fragment.children[1];

  let expected_height = expected_baseline_track_size([a, b]);
  assert_approx(fragment.bounds.height(), expected_height, "grid row height");

  let max_item_height = a.bounds.height().max(b.bounds.height());
  assert!(
    fragment.bounds.height() > max_item_height + 0.5,
    "expected baseline alignment to increase row height beyond either item (row={:.2}, max_item={:.2})",
    fragment.bounds.height(),
    max_item_height
  );

  let baseline_a = a.bounds.y() + baseline_offset_with_fallback(a);
  let baseline_b = b.bounds.y() + baseline_offset_with_fallback(b);
  assert!(
    (baseline_a - baseline_b).abs() <= 0.5,
    "expected baselines to align (a={baseline_a:.2}, b={baseline_b:.2})"
  );
}
