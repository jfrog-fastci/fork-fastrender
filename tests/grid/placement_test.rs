use fastrender::css::properties::parse_property_value;
use fastrender::css::types::{Declaration, PropertyValue};
use fastrender::layout::constraints::LayoutConstraints;
use fastrender::layout::contexts::grid::GridFormattingContext;
use fastrender::layout::formatting_context::FormattingContext;
use fastrender::style::display::{Display, FormattingContextType};
use fastrender::style::properties::apply_declaration;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;
use fastrender::tree::box_tree::BoxNode;
use std::sync::Arc;

fn decl(property: &'static str, css_text: &str) -> Declaration {
  let value = parse_property_value(property, css_text).expect("parse property value");
  let contains_var = match &value {
    PropertyValue::Keyword(raw) | PropertyValue::Custom(raw) => {
      fastrender::style::var_resolution::contains_var(raw)
    }
    _ => false,
  };
  Declaration {
    property: property.into(),
    value,
    contains_var,
    raw_value: css_text.to_string(),
    important: false,
  }
}

fn assert_approx(actual: f32, expected: f32, msg: &str) {
  assert!(
    (actual - expected).abs() <= 0.5,
    "{msg}: got {actual} expected {expected}"
  );
}

#[test]
fn grid_column_longhand_inherit_preserves_other_side() {
  // Regression: `grid-column-start/end: inherit` must copy only that longhand from the parent,
  // leaving the other side untouched.
  let base = ComputedStyle::default();

  let mut parent = ComputedStyle::default();
  apply_declaration(&mut parent, &decl("grid-column-start", "2"), &base, 16.0, 16.0);
  apply_declaration(&mut parent, &decl("grid-column-end", "4"), &base, 16.0, 16.0);

  let mut child = ComputedStyle::default();
  apply_declaration(&mut child, &decl("grid-column-end", "3"), &base, 16.0, 16.0);
  apply_declaration(
    &mut child,
    &decl("grid-column-start", "inherit"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(child.grid_column_raw.as_deref(), Some("2 / 3"));

  let mut child2 = ComputedStyle::default();
  apply_declaration(&mut child2, &decl("grid-column-start", "1"), &base, 16.0, 16.0);
  apply_declaration(
    &mut child2,
    &decl("grid-column-end", "inherit"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(child2.grid_column_raw.as_deref(), Some("1 / 4"));
}

#[test]
fn grid_row_longhand_inherit_preserves_other_side() {
  let base = ComputedStyle::default();

  let mut parent = ComputedStyle::default();
  apply_declaration(&mut parent, &decl("grid-row-start", "2"), &base, 16.0, 16.0);
  apply_declaration(&mut parent, &decl("grid-row-end", "4"), &base, 16.0, 16.0);

  let mut child = ComputedStyle::default();
  apply_declaration(&mut child, &decl("grid-row-end", "3"), &base, 16.0, 16.0);
  apply_declaration(
    &mut child,
    &decl("grid-row-start", "inherit"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(child.grid_row_raw.as_deref(), Some("2 / 3"));

  let mut child2 = ComputedStyle::default();
  apply_declaration(&mut child2, &decl("grid-row-start", "1"), &base, 16.0, 16.0);
  apply_declaration(
    &mut child2,
    &decl("grid-row-end", "inherit"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(child2.grid_row_raw.as_deref(), Some("1 / 4"));
}

#[test]
fn grid_area_auto_is_auto_placement() {
  // Regression test: `grid-area: auto` is the initial value and must not be treated as a named
  // area ("auto-start"/"auto-end").
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(30.0));
  grid_style.height = Some(Length::px(10.0));
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-columns", "10px 20px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-rows", "10px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-areas", "\"a b\""),
    &base,
    16.0,
    16.0,
  );

  let mut a_style = ComputedStyle::default();
  a_style.display = Display::Block;
  apply_declaration(&mut a_style, &decl("grid-area", "a"), &base, 16.0, 16.0);

  let mut auto_style = ComputedStyle::default();
  auto_style.display = Display::Block;
  apply_declaration(
    &mut auto_style,
    &decl("grid-area", "auto"),
    &base,
    16.0,
    16.0,
  );

  let child_a = BoxNode::new_block(Arc::new(a_style), FormattingContextType::Block, vec![]);
  let child_auto = BoxNode::new_block(Arc::new(auto_style), FormattingContextType::Block, vec![]);

  let grid = BoxNode::new_block(
    Arc::new(grid_style),
    FormattingContextType::Grid,
    vec![child_a, child_auto],
  );

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(30.0, 10.0))
    .expect("grid layout succeeds");

  assert_eq!(fragment.children.len(), 2);
  let placed_a = &fragment.children[0];
  let placed_auto = &fragment.children[1];

  assert_approx(placed_a.bounds.x(), 0.0, "area a x");
  assert_approx(placed_a.bounds.width(), 10.0, "area a width");
  assert_approx(placed_auto.bounds.x(), 10.0, "auto item x");
  assert_approx(placed_auto.bounds.width(), 20.0, "auto item width");
}

#[test]
fn grid_row_start_named_area_name_resolves_to_area_start_line() {
  // Spec example: `grid-row-start: main` aligns the row-start edge to the start edge of the named
  // area.
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(70.0));
  grid_style.height = Some(Length::px(30.0));
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-columns", "30px 40px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-rows", "10px 20px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-areas", "\"a c\" \"b d\""),
    &base,
    16.0,
    16.0,
  );

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  apply_declaration(&mut item_style, &decl("grid-row-start", "b"), &base, 16.0, 16.0);

  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
  let grid = BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, vec![item]);

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(70.0, 30.0))
    .expect("grid layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let placed = &fragment.children[0];
  assert_approx(placed.bounds.y(), 10.0, "row-start aligned to area b start (row 2)");
  assert_approx(placed.bounds.height(), 20.0, "row height");
}

#[test]
fn grid_column_start_named_area_name_resolves_to_area_start_line() {
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(70.0));
  grid_style.height = Some(Length::px(30.0));
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-columns", "30px 40px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-rows", "10px 20px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-areas", "\"a c\" \"b d\""),
    &base,
    16.0,
    16.0,
  );

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  apply_declaration(
    &mut item_style,
    &decl("grid-column-start", "d"),
    &base,
    16.0,
    16.0,
  );

  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
  let grid = BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, vec![item]);

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(70.0, 30.0))
    .expect("grid layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let placed = &fragment.children[0];
  assert_approx(
    placed.bounds.x(),
    30.0,
    "column-start aligned to area d start (column 2)",
  );
  assert_approx(placed.bounds.width(), 40.0, "column width");
}

#[test]
fn grid_area_span_expands_implicit_tracks_and_includes_gaps() {
  // Matches the spec's `grid-area: span 2 / span 3` auto-placement pattern.
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  // Choose a container size that exactly fits the explicit + implicit tracks and gaps.
  grid_style.width = Some(Length::px(55.0));
  grid_style.height = Some(Length::px(21.0));
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-columns", "10px 15px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-rows", "7px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-auto-columns", "20px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-auto-rows", "11px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("column-gap", "5px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("row-gap", "3px"),
    &base,
    16.0,
    16.0,
  );

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  apply_declaration(
    &mut item_style,
    &decl("grid-area", "span 2 / span 3"),
    &base,
    16.0,
    16.0,
  );

  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
  let grid = BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, vec![item]);

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(55.0, 21.0))
    .expect("grid layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let placed = &fragment.children[0];
  assert_approx(placed.bounds.x(), 0.0, "auto placed x");
  assert_approx(placed.bounds.y(), 0.0, "auto placed y");
  assert_approx(
    placed.bounds.width(),
    55.0,
    "span 3 columns includes implicit track + gaps",
  );
  assert_approx(
    placed.bounds.height(),
    21.0,
    "span 2 rows includes implicit track + gaps",
  );
}

#[test]
fn grid_area_span_with_center_justify_content_respects_gaps() {
  // Same grid as above but centered within a larger container. This exercises the interaction
  // between spanning items, implicit tracks, gaps, and `justify-content`.
  let base = ComputedStyle::default();

  let mut grid_style = ComputedStyle::default();
  grid_style.display = Display::Grid;
  grid_style.width = Some(Length::px(100.0));
  grid_style.height = Some(Length::px(21.0));
  apply_declaration(
    &mut grid_style,
    &decl("justify-content", "center"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-columns", "10px 15px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-template-rows", "7px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-auto-columns", "20px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("grid-auto-rows", "11px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("column-gap", "5px"),
    &base,
    16.0,
    16.0,
  );
  apply_declaration(
    &mut grid_style,
    &decl("row-gap", "3px"),
    &base,
    16.0,
    16.0,
  );

  let mut item_style = ComputedStyle::default();
  item_style.display = Display::Block;
  apply_declaration(
    &mut item_style,
    &decl("grid-area", "span 2 / span 3"),
    &base,
    16.0,
    16.0,
  );

  let item = BoxNode::new_block(Arc::new(item_style), FormattingContextType::Block, vec![]);
  let grid = BoxNode::new_block(Arc::new(grid_style), FormattingContextType::Grid, vec![item]);

  let fc = GridFormattingContext::new();
  let fragment = fc
    .layout(&grid, &LayoutConstraints::definite(100.0, 21.0))
    .expect("grid layout succeeds");

  assert_eq!(fragment.children.len(), 1);
  let placed = &fragment.children[0];
  // Track size is 55px, leftover is 45px => centered offset 22.5px.
  assert_approx(placed.bounds.x(), 22.5, "justify-content center offset");
  assert_approx(placed.bounds.width(), 55.0, "centered span width");
}
