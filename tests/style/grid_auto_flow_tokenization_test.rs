use fastrender::css::properties::parse_property_value;
use fastrender::css::types::Declaration;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::GridAutoFlow;
use fastrender::ComputedStyle;

fn decl(name: &'static str, value: &str) -> Declaration {
  let contains_var = fastrender::style::var_resolution::contains_var(value);
  Declaration {
    property: name.into(),
    value: parse_property_value(name, value).expect("parse property value"),
    contains_var,
    raw_value: value.to_string(),
    important: false,
  }
}

#[test]
fn grid_auto_flow_keywords_are_ascii_case_insensitive() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("grid-auto-flow", "COLUMN DENSE"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.grid_auto_flow, GridAutoFlow::ColumnDense);
}

#[test]
fn grid_auto_flow_rejects_invalid_identifiers_instead_of_substring_matching() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("grid-auto-flow", "column"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.grid_auto_flow, GridAutoFlow::Column);

  // `rowdense` is a single identifier, not the two keywords `row dense`, and should therefore be
  // treated as invalid (ignored).
  apply_declaration(
    &mut styles,
    &decl("grid-auto-flow", "rowdense"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.grid_auto_flow, GridAutoFlow::Column);

  // Likewise, specifying both `row` and `column` is invalid.
  apply_declaration(
    &mut styles,
    &decl("grid-auto-flow", "row column"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert_eq!(styles.grid_auto_flow, GridAutoFlow::Column);
}

