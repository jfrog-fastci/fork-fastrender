use fastrender::layout::constraints::AvailableSpace;
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::types::Overflow;
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
fn grid_nested_grid_container_has_nonzero_intrinsic_block_size() {
  // Regression test for nested grid containers participating in intrinsic track sizing.
  //
  // Some pages (e.g. wired.com's sticky nav) wrap a grid container inside another grid whose rows
  // are sized with `fr` units while the outer container's block size is indefinite. In that case,
  // `fr` tracks behave like content-sized tracks, so the outer grid must query the nested grid
  // container's intrinsic block size.
  //
  // If the nested grid reports a bogus 0px intrinsic size, the outer `1fr` row collapses to 0px,
  // and `overflow:hidden` on the nested grid clips its in-flow descendants.

  let mut outer_style = ComputedStyle::default();
  outer_style.display = Display::Grid;
  outer_style.width = Some(Length::px(100.0));
  outer_style.grid_template_columns = vec![GridTrack::Length(Length::px(100.0))];
  outer_style.grid_template_rows = vec![GridTrack::Fr(1.0)];

  // Simulate the two-column nav row on wired.com:
  // - left area is taller (80px)
  // - right area is shorter (66px)
  let make_flex_item = |height: f32| {
    let mut child_style = ComputedStyle::default();
    child_style.display = Display::Block;
    child_style.height = Some(Length::px(height));
    let child = BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![]);

    let mut flex_style = ComputedStyle::default();
    flex_style.display = Display::Flex;
    BoxNode::new_block(
      Arc::new(flex_style),
      FormattingContextType::Flex,
      vec![child],
    )
  };

  let mut inner_style = ComputedStyle::default();
  inner_style.display = Display::Grid;
  inner_style.grid_template_columns = vec![GridTrack::Fr(3.0), GridTrack::Fr(1.0)];
  inner_style.grid_template_rows = vec![GridTrack::Auto];
  inner_style.overflow_x = Overflow::Hidden;
  inner_style.overflow_y = Overflow::Hidden;
  let inner_grid = BoxNode::new_block(
    Arc::new(inner_style),
    FormattingContextType::Grid,
    vec![make_flex_item(80.0), make_flex_item(66.0)],
  );

  let grid = BoxNode::new_block(
    Arc::new(outer_style),
    FormattingContextType::Grid,
    vec![inner_grid],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(
      &grid,
      &LayoutConstraints::new(AvailableSpace::Definite(100.0), AvailableSpace::Indefinite),
    )
    .expect("layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let inner_fragment = fragment.children.first().expect("inner grid fragment");

  assert_approx(fragment.bounds.height(), 80.0, "outer grid height");
  assert_approx(inner_fragment.bounds.height(), 80.0, "inner grid height");
}
