use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::layout::formatting_context::IntrinsicSizingMode;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::tree::box_tree::BoxNode;
use fastrender::ComputedStyle;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_percent_height_computes_to_auto_when_containing_block_height_is_indefinite() {
  // CSS2.1 §10.5: Percentage `height` values compute to `auto` when the containing block height is
  // not specified explicitly. This comes up with sticky headers that set `height: 100%` on grid
  // rows even though their flex containers have an indefinite height.

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(100.0));
  grid_style.height = Some(Length::percent(100.0));
  grid_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  grid_style.grid_template_rows = vec![GridTrack::Fr(1.0)];

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.height = Some(Length::px(80.0));
  let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

  let mut grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child],
  );
  grid.id = 1;

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(
      &grid,
      &LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  assert_approx(fragment.bounds.height(), 80.0, "grid height");
  assert_approx(
    fragment.children[0].bounds.height(),
    80.0,
    "grid item height",
  );

  // This same scenario occurs during flex/grid intrinsic sizing probes: the max-content intrinsic
  // block size must not collapse to 0 when the only authored height is a percentage.
  let intrinsic_block = fc
    .compute_intrinsic_block_size(&grid, IntrinsicSizingMode::MaxContent)
    .expect("intrinsic sizing succeeds");
  assert_approx(intrinsic_block, 80.0, "grid intrinsic block size");
}
