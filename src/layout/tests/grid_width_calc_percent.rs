use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::values::CalcLength;
use fastrender::style::values::Length;
use fastrender::style::values::LengthUnit;
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
use std::sync::Arc;

fn calc_percent_plus_px(percent: f32, px: f32) -> Length {
  let calc = CalcLength::single(LengthUnit::Percent, percent)
    .add_scaled(&CalcLength::single(LengthUnit::Px, px), 1.0)
    .expect("calc expression should be representable");
  Length::calc(calc)
}

#[test]
fn grid_container_width_calc_with_percentage_resolves_against_containing_block_width() {
  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(calc_percent_plus_px(10.0, 5.0)); // calc(10% + 5px)
  grid_style.width_keyword = None;
  // Ensure the container has a definite block-size so Taffy doesn't stretch it to available space.
  grid_style.height = Some(Length::px(10.0));
  grid_style.height_keyword = None;

  let grid = BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, vec![]);

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(200.0, 50.0))
    .expect("layout succeeds");

  assert!(
    (fragment.bounds.width() - 25.0).abs() < 0.5,
    "expected border-box width≈25, got width={}",
    fragment.bounds.width()
  );
}
