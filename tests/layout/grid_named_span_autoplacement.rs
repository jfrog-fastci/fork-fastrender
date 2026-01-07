use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::Display;
use fastrender::style::display::FormattingContextType;
use fastrender::style::types::GridTrack;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn make_named_grid_child(column_raw: &str, row_raw: &str) -> BoxNode {
  let mut child_style = ComputedStyle::default();
  child_style.display = Display::Block;
  child_style.grid_column_raw = Some(column_raw.to_string());
  child_style.grid_row_raw = Some(row_raw.to_string());
  BoxNode::new_block(Arc::new(child_style), FormattingContextType::Block, vec![])
}

fn make_named_grid_container(child: BoxNode) -> BoxNode {
  let mut container_style = ComputedStyle::default();
  container_style.display = Display::Grid;
  container_style.width = Some(Length::px(100.0));
  container_style.height = Some(Length::px(80.0));
  container_style.grid_template_columns = vec![
    GridTrack::Length(Length::px(50.0)),
    GridTrack::Length(Length::px(50.0)),
  ];
  container_style.grid_template_rows = vec![
    GridTrack::Length(Length::px(40.0)),
    GridTrack::Length(Length::px(40.0)),
  ];

  // Name every grid line "foo" so that `span foo 2` corresponds to spanning two tracks in either
  // direction while still exercising the named-line occurrence search.
  container_style.grid_column_line_names =
    vec![vec!["foo".to_string()], vec!["foo".to_string()], vec!["foo".to_string()]];
  container_style.grid_row_line_names =
    vec![vec!["foo".to_string()], vec!["foo".to_string()], vec!["foo".to_string()]];

  BoxNode::new_block(
    Arc::new(container_style),
    FormattingContextType::Grid,
    vec![child],
  )
}

#[test]
fn grid_named_span_autoplacement_auto_then_named_span() {
  let container = make_named_grid_container(make_named_grid_child(
    "auto / span foo 2",
    "auto / span foo 2",
  ));
  let grid_fc = GridFormattingContext::new();
  let fragment = grid_fc
    .layout(&container, &LayoutConstraints::definite(100.0, 80.0))
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  assert!(
    (child.bounds.width() - 100.0).abs() < 0.1,
    "expected item to span both columns (got width {})",
    child.bounds.width()
  );
  assert!(
    (child.bounds.height() - 80.0).abs() < 0.1,
    "expected item to span both rows (got height {})",
    child.bounds.height()
  );
}

#[test]
fn grid_named_span_autoplacement_named_span_then_auto() {
  let container = make_named_grid_container(make_named_grid_child(
    "span foo 2 / auto",
    "span foo 2 / auto",
  ));
  let grid_fc = GridFormattingContext::new();
  let fragment = grid_fc
    .layout(&container, &LayoutConstraints::definite(100.0, 80.0))
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  assert!(
    (child.bounds.width() - 100.0).abs() < 0.1,
    "expected item to span both columns (got width {})",
    child.bounds.width()
  );
  assert!(
    (child.bounds.height() - 80.0).abs() < 0.1,
    "expected item to span both rows (got height {})",
    child.bounds.height()
  );
}

#[test]
fn grid_named_span_conflict_handling_span_then_named_span() {
  let container = make_named_grid_container(make_named_grid_child(
    "span 2 / span foo 2",
    "span 2 / span foo 2",
  ));
  let grid_fc = GridFormattingContext::new();
  let fragment = grid_fc
    .layout(&container, &LayoutConstraints::definite(100.0, 80.0))
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  assert!(
    (child.bounds.width() - 100.0).abs() < 0.1,
    "expected end span to be ignored (span 2 should remain) (got width {})",
    child.bounds.width()
  );
  assert!(
    (child.bounds.height() - 80.0).abs() < 0.1,
    "expected end span to be ignored (span 2 should remain) (got height {})",
    child.bounds.height()
  );
}

#[test]
fn grid_named_span_conflict_handling_named_span_then_span() {
  let container = make_named_grid_container(make_named_grid_child(
    "span foo 2 / span 2",
    "span foo 2 / span 2",
  ));
  let grid_fc = GridFormattingContext::new();
  let fragment = grid_fc
    .layout(&container, &LayoutConstraints::definite(100.0, 80.0))
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  assert!(
    (child.bounds.width() - 50.0).abs() < 0.1,
    "expected named span to resolve to the default span 1 (got width {})",
    child.bounds.width()
  );
  assert!(
    (child.bounds.height() - 40.0).abs() < 0.1,
    "expected named span to resolve to the default span 1 (got height {})",
    child.bounds.height()
  );
}

#[test]
fn grid_named_span_conflict_handling_named_span_then_named_span() {
  let container = make_named_grid_container(make_named_grid_child(
    "span foo 2 / span foo 2",
    "span foo 2 / span foo 2",
  ));
  let grid_fc = GridFormattingContext::new();
  let fragment = grid_fc
    .layout(&container, &LayoutConstraints::definite(100.0, 80.0))
    .expect("layout should succeed");

  let child = fragment.children.first().expect("child fragment");
  assert!(
    (child.bounds.width() - 50.0).abs() < 0.1,
    "expected placement with only a named span to fall back to span 1 (got width {})",
    child.bounds.width()
  );
  assert!(
    (child.bounds.height() - 40.0).abs() < 0.1,
    "expected placement with only a named span to fall back to span 1 (got height {})",
    child.bounds.height()
  );
}
