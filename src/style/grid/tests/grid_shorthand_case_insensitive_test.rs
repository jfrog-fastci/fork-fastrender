use crate::css::properties::parse_property_value;
use crate::css::types::Declaration;
use crate::style::properties::apply_declaration;
use crate::style::types::GridAutoFlow;
use crate::style::types::GridTrack;
use crate::style::values::Length;
use crate::ComputedStyle;

fn decl(name: &'static str, value: &str) -> Declaration {
  let contains_var = crate::style::var_resolution::contains_var(value);
  Declaration {
    property: name.into(),
    value: parse_property_value(name, value).expect("parse property value"),
    contains_var,
    raw_value: value.to_string(),
    important: false,
  }
}

#[test]
fn grid_shorthand_auto_flow_keywords_are_case_insensitive() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("grid", "AUTO-FLOW CoLuMn DeNsE 10px / 20px"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.grid_auto_flow, GridAutoFlow::ColumnDense);
  assert_eq!(
    styles.grid_auto_rows[0],
    GridTrack::Length(Length::px(10.0))
  );
  assert_eq!(
    styles.grid_auto_columns[0],
    GridTrack::Length(Length::px(20.0))
  );
}

#[test]
fn grid_shorthand_none_is_case_insensitive() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("grid", "NoNe"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  // `grid: none` resets the template + implicit track lists.
  assert!(styles.grid_template_rows.is_empty());
  assert!(styles.grid_template_columns.is_empty());
  assert!(styles.grid_template_areas.is_empty());

  assert_eq!(styles.grid_auto_flow, GridAutoFlow::Row);
}
