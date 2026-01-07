use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::BoxNode;
use fastrender::ComputedStyle;
use fastrender::FormattingContextType;
use std::sync::Arc;

fn assert_approx(val: f32, expected: f32, msg: &str) {
  assert!(
    (val - expected).abs() <= 0.5,
    "{msg}: got {val} expected {expected}",
  );
}

#[test]
fn grid_percent_tracks_with_definite_container_size_resolve() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(400.0));
  container_style.height = Some(Length::px(200.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::percent(25.0)),
    GridTrack::Length(Length::percent(75.0)),
  ];
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::percent(30.0)),
    GridTrack::Length(Length::percent(70.0)),
  ];
  let container_style = Arc::new(container_style);

  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  let child_style = Arc::new(child_style);

  let mut grid = BoxNode::new_block(
    container_style,
    FormattingContextType::Grid,
    vec![
      BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]),
      BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]),
      BoxNode::new_block(child_style.clone(), FormattingContextType::Block, vec![]),
      BoxNode::new_block(child_style, FormattingContextType::Block, vec![]),
    ],
  );
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(800.0, 600.0))
    .expect("layout succeeds");

  assert_approx(fragment.bounds.width(), 400.0, "grid width");
  assert_approx(fragment.bounds.height(), 200.0, "grid height");

  assert_eq!(fragment.children.len(), 4);
  let c0 = &fragment.children[0];
  let c1 = &fragment.children[1];
  let c2 = &fragment.children[2];
  let c3 = &fragment.children[3];

  assert_approx(c0.bounds.x(), 0.0, "cell 0 x");
  assert_approx(c0.bounds.y(), 0.0, "cell 0 y");
  assert_approx(c0.bounds.width(), 100.0, "cell 0 width (25%)");
  assert_approx(c0.bounds.height(), 60.0, "cell 0 height (30%)");

  assert_approx(c1.bounds.x(), 100.0, "cell 1 x");
  assert_approx(c1.bounds.y(), 0.0, "cell 1 y");
  assert_approx(c1.bounds.width(), 300.0, "cell 1 width (75%)");
  assert_approx(c1.bounds.height(), 60.0, "cell 1 height (30%)");

  assert_approx(c2.bounds.x(), 0.0, "cell 2 x");
  assert_approx(c2.bounds.y(), 60.0, "cell 2 y");
  assert_approx(c2.bounds.width(), 100.0, "cell 2 width (25%)");
  assert_approx(c2.bounds.height(), 140.0, "cell 2 height (70%)");

  assert_approx(c3.bounds.x(), 100.0, "cell 3 x");
  assert_approx(c3.bounds.y(), 60.0, "cell 3 y");
  assert_approx(c3.bounds.width(), 300.0, "cell 3 width (75%)");
  assert_approx(c3.bounds.height(), 140.0, "cell 3 height (70%)");
}

