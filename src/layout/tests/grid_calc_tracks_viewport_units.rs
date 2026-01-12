use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::types::GridTrack;
use crate::style::values::CalcLength;
use crate::style::values::Length;
use crate::style::values::LengthUnit;
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
fn grid_track_fixed_size_resolves_calc_with_viewport_units() {
  // Grid templates can include `calc()` lengths that depend on viewport units (e.g. `100vw`).
  // Taffy expects fixed track sizes as concrete lengths; when we fail to resolve `calc()` here we
  // fall back to `Length::to_px()` which treats unresolved units as raw numbers.
  //
  // Example: `calc(100vw - 170px)` in an 800px-wide viewport should resolve to `630px`, not
  // `100 - 170 = -70px`.
  let fc = GridFormattingContext::new();

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(10.0));
  child_style.height_keyword = None;
  let child_style = Arc::new(child_style);

  let mut calc = CalcLength::single(LengthUnit::Vw, 100.0);
  calc = calc
    .add_scaled(&CalcLength::single(LengthUnit::Px, 170.0), -1.0)
    .expect("calc term limit");
  let track_len = Length::calc(calc);

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(800.0));
  grid_style.width_keyword = None;
  grid_style.grid_template_columns = vec![GridTrack::Length(track_len)];
  let grid_style = Arc::new(grid_style);

  let mut grid = BoxNode::new_block(
    grid_style,
    FormattingContextType::Grid,
    vec![BoxNode::new_block(
      child_style,
      FormattingContextType::Block,
      vec![],
    )],
  );
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(800.0, 600.0))
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let cell = &fragment.children[0];
  assert_approx(cell.bounds.width(), 630.0, "grid column width");
}
