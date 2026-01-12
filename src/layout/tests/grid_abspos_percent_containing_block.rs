use crate::layout::constraints::LayoutConstraints;
use crate::layout::contexts::grid::GridFormattingContext;
use crate::layout::formatting_context::FormattingContext;
use crate::style::display::Display;
use crate::style::display::FormattingContextType;
use crate::style::position::Position;
use crate::style::types::GridTrack;
use crate::style::values::{Length, LengthUnit};
use crate::style::ComputedStyle;
use crate::tree::box_tree::BoxNode;
use std::sync::Arc;

#[test]
fn abspos_grid_child_percent_sizing_uses_grid_area_containing_block() {
  // CSS Grid § 10.1 "Absolutely Positioned Items": when an abspos child of a grid container has
  // grid placement, its containing block is the grid area defined by those grid lines. Percentage
  // sizing and inset offsets must resolve against the grid area's used size, not the grid
  // container's padding box.
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.position = Position::Relative;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(50.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(60.0)),
  ];
  container_style.grid_template_rows = vec![GridTrack::Length(Length::px(50.0))];

  // Ensure Taffy computes detailed grid track info by including at least one in-flow grid item.
  // (Out-of-flow positioned children are removed from the Taffy tree.)
  let mut flow_style = ComputedStyle::default();
  flow_style.display = Display::Block;
  flow_style.width = Some(Length::px(1.0));
  flow_style.height = Some(Length::px(1.0));
  let flow_child = BoxNode::new_block(Arc::new(flow_style), FormattingContextType::Block, vec![]);

  let mut abs_style = ComputedStyle::default();
  abs_style.display = Display::Block;
  abs_style.position = Position::Absolute;
  abs_style.grid_column_start = 2;
  abs_style.grid_column_end = 3;
  abs_style.grid_row_start = 1;
  abs_style.grid_row_end = 2;
  abs_style.width = Some(Length::new(100.0, LengthUnit::Percent));
  abs_style.height = Some(Length::new(100.0, LengthUnit::Percent));

  let abs_child = BoxNode::new_block(Arc::new(abs_style), FormattingContextType::Block, vec![]);
  let container = BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![flow_child, abs_child],
  );

  let constraints = LayoutConstraints::definite(100.0, 50.0);
  let fc = GridFormattingContext::new();
  let fragment = fc.layout(&container, &constraints).expect("grid layout");

  let abs_fragment = fragment
    .iter_fragments()
    .find(|node| {
      matches!(
        node.style.as_ref().map(|s| s.position),
        Some(Position::Absolute)
      )
    })
    .expect("absolute fragment present");

  assert!(
    (abs_fragment.bounds.x() - 40.0).abs() < 0.1,
    "expected abspos x≈40 (start of second column), got {}",
    abs_fragment.bounds.x()
  );
  assert!(
    (abs_fragment.bounds.width() - 60.0).abs() < 0.1,
    "expected abspos width≈60 (width of second column), got {}",
    abs_fragment.bounds.width()
  );
  assert!(
    (abs_fragment.bounds.height() - 50.0).abs() < 0.1,
    "expected abspos height≈50 (height of first row), got {}",
    abs_fragment.bounds.height()
  );
}
