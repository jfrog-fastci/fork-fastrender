use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::types::{AlignItems, GridTrack};
use crate::style::values::Length;
use crate::{BoxNode, ComputedStyle, FormattingContextType};
use std::sync::Arc;

fn assert_approx(value: f32, expected: f32, what: &str) {
  assert!(
    (value - expected).abs() < 0.5,
    "expected {what} to be {expected:.1}px (got {value:.1}px)"
  );
}

#[test]
fn grid_item_percent_min_width_uses_grid_area() {
  let fc = GridFormattingContext::new();

  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(400.0));
  container_style.height = Some(Length::px(20.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(100.0)),
    GridTrack::Length(Length::px(300.0)),
  ];
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(20.0))];
  container_style.justify_items = AlignItems::Stretch;

  let mut first_style = ComputedStyle::default();
  first_style.display = Display::Block;

  let mut second_style = ComputedStyle::default();
  second_style.display = Display::Block;
  second_style.min_width = Some(Length::percent(100.0));
  second_style.justify_self = Some(AlignItems::Stretch);

  let mut first = BoxNode::new_block(Arc::new(first_style), FormattingContextType::Block, vec![]);
  first.id = 2;
  let mut second = BoxNode::new_block(Arc::new(second_style), FormattingContextType::Block, vec![]);
  second.id = 3;

  let mut grid = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![first, second],
  );
  grid.id = 1;

  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(400.0, 20.0))
    .expect("layout succeeds");

  assert_approx(fragment.bounds.width(), 400.0, "grid width");
  assert_eq!(fragment.children.len(), 2);

  let second_fragment = &fragment.children[1];
  assert_approx(second_fragment.bounds.x(), 100.0, "second item x");
  assert_approx(second_fragment.bounds.width(), 300.0, "second item width");
  let right_edge = second_fragment.bounds.x() + second_fragment.bounds.width();
  assert!(
    right_edge <= 400.5,
    "grid item should not overflow its grid area: right edge {right_edge}",
  );
}
