use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::types::IntrinsicSizeKeyword;
use fastrender::style::types::WordBreak;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_fit_content_track_clamps_to_argument() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(40.0));
  container_style.grid_template_columns =
    vec![GridTrack::FitContent(Length::px(100.0)), GridTrack::Fr(1.0)];
  container_style.grid_template_rows = vec![GridTrack::Auto];
  let container_style = Arc::new(container_style);

  // `fit-content(100px)` clamps between min-content and max-content sizes. Use break-all text so
  // min-content is small but max-content is large, making the clamp observable.
  let long_text = "a".repeat(64);
  let mut wide_style = ComputedStyle::default();
  wide_style.display = Display::Block;
  wide_style.font_size = 16.0;
  wide_style.word_break = WordBreak::BreakAll;
  wide_style.grid_column_start = 1;
  wide_style.grid_column_end = 2;
  let wide_style = Arc::new(wide_style);

  let mut text_style = ComputedStyle::default();
  text_style.font_size = 16.0;
  text_style.word_break = WordBreak::BreakAll;
  let text_child = BoxNode::new_text(Arc::new(text_style), long_text);
  let wide = BoxNode::new_block(wide_style, FormattingContextType::Inline, vec![text_child]);

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.width = Some(Length::px(10.0));
  second_style.height = Some(Length::px(10.0));
  second_style.grid_column_start = 2;
  second_style.grid_column_end = 3;
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![wide, second],
  );

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 40.0))
    .expect("grid layout");

  assert_approx(
    fragment.children[1].bounds.x(),
    100.0,
    "second column start",
  );
}

#[test]
fn grid_minmax_minimum_affects_flex_distribution() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(150.0));
  container_style.height = Some(Length::px(20.0));
  container_style.grid_template_columns = vec![
    GridTrack::MinMax(
      Box::new(GridTrack::Length(Length::px(80.0))),
      Box::new(GridTrack::Fr(1.0)),
    ),
    GridTrack::Fr(1.0),
  ];
  container_style.grid_template_rows = vec![GridTrack::Auto];
  let container_style = Arc::new(container_style);

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  first_style.width = Some(Length::px(10.0));
  first_style.height = Some(Length::px(10.0));
  first_style.grid_column_start = 1;
  first_style.grid_column_end = 2;
  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.width = Some(Length::px(10.0));
  second_style.height = Some(Length::px(10.0));
  second_style.grid_column_start = 2;
  second_style.grid_column_end = 3;
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![first, second],
  );

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(150.0, 20.0))
    .expect("grid layout");

  // 150px total width with a min of 80px on the first flex track should leave <= 70px for the
  // second track.
  assert_approx(fragment.children[1].bounds.x(), 80.0, "second column start");
}

#[test]
fn grid_spanning_item_contributes_to_intrinsic_width() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width_keyword = Some(IntrinsicSizeKeyword::MaxContent);
  container_style.grid_template_columns = vec![GridTrack::Auto, GridTrack::Auto];
  container_style.grid_template_rows = vec![GridTrack::Auto];
  let container_style = Arc::new(container_style);

  let mut span_style = ComputedStyle::default();
  span_style.display = Display::Block;
  span_style.width = Some(Length::px(100.0));
  span_style.height = Some(Length::px(10.0));
  span_style.grid_column_start = 1;
  span_style.grid_column_end = 3;
  let span_item = BoxNode::new_block(Arc::new(span_style), FormattingContextType::Block, vec![]);

  let mut grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![span_item],
  );
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(500.0, 200.0))
    .expect("grid layout");

  assert_approx(
    fragment.bounds.width(),
    100.0,
    "intrinsic max-content width",
  );
}
