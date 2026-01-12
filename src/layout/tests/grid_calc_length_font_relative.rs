use crate::css::properties::parse_length;
use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::BoxNode;
use crate::ComputedStyle;
use crate::FormattingContextType;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_resolves_calc_lengths_with_rem_units_in_track_sizing() {
  let calc = parse_length("calc(5.625rem + 1px)").expect("calc length parses");

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.grid_template_rows = vec![GridTrack::Length(calc), GridTrack::Auto];
  grid_style.grid_template_columns = vec![GridTrack::Auto];
  grid_style.width = Some(Length::px(200.0));
  grid_style.font_size = 16.0;
  grid_style.root_font_size = 16.0;

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;
  // Let the item stretch to the fixed track size so we can observe the track resolution result.
  first_style.font_size = 16.0;
  first_style.root_font_size = 16.0;

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.height = Some(Length::px(10.0));
  second_style.font_size = 16.0;
  second_style.root_font_size = 16.0;

  let first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  let second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![first, second],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 200.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 2);
  let first_fragment = &fragment.children[0];
  let second_fragment = &fragment.children[1];

  // `5.625rem` with a 16px root font size is exactly 90px, so the calc is 91px.
  assert_approx(
    first_fragment.bounds.height(),
    91.0,
    "first grid row track size",
  );
  assert_approx(second_fragment.bounds.y(), 91.0, "second row offset");
  assert_approx(second_fragment.bounds.height(), 10.0, "second row height");
}
