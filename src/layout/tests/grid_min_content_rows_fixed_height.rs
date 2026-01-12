use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
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
fn grid_min_content_rows_respect_fixed_height_items() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(200.0));
  container_style.height = Some(Length::px(200.0));
  container_style.grid_template_columns = vec![GridTrack::Fr(1.0)];
  container_style.grid_template_rows = vec![GridTrack::MinContent, GridTrack::MinContent];
  let container_style = Arc::new(container_style);

  // Use a calc length that depends on root font size (rem). This matches the CSS on MDN's
  // `page-layout__banner` which uses `calc(<rem> + <px>)`.
  let mut first_style = ComputedStyle::default();
  let root_font_size = first_style.root_font_size;
  let calc_height = CalcLength::single(LengthUnit::Rem, 5.0)
    .add_scaled(&CalcLength::single(LengthUnit::Px, 1.0), 1.0)
    .expect("calc height terms");
  let expected_height = 5.0 * root_font_size + 1.0;
  first_style.display = Display::Block;
  first_style.height = Some(Length::calc(calc_height));
  first_style.grid_row_start = 1;
  first_style.grid_row_end = 2;
  first_style.grid_column_start = 1;
  first_style.grid_column_end = 2;
  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.height = Some(Length::px(10.0));
  second_style.grid_row_start = 2;
  second_style.grid_row_end = 3;
  second_style.grid_column_start = 1;
  second_style.grid_column_end = 2;
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![first, second],
  );

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("grid layout");

  assert_approx(
    fragment.children[1].bounds.y(),
    expected_height,
    "second row start",
  );
}
