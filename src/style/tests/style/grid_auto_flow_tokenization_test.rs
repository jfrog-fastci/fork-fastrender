use fastrender::css::types::Declaration;
use fastrender::css::types::PropertyValue;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::GridAutoFlow;
use fastrender::style::ComputedStyle;

fn decl(name: &'static str, value: PropertyValue) -> Declaration {
  let contains_var = match &value {
    PropertyValue::Keyword(raw) | PropertyValue::Custom(raw) => {
      fastrender::style::var_resolution::contains_var(raw)
    }
    _ => false,
  };
  Declaration {
    property: name.into(),
    value,
    contains_var,
    raw_value: String::new(),
    important: false,
  }
}

#[test]
fn grid_auto_flow_tokenizes_whitespace_comments_and_is_case_insensitive() {
  let mut style = ComputedStyle::default();
  let base = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid-auto-flow", PropertyValue::Keyword("row".into())),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_auto_flow, GridAutoFlow::Row);

  apply_declaration(
    &mut style,
    &decl("grid-auto-flow", PropertyValue::Keyword("CoLuMn".into())),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_auto_flow, GridAutoFlow::Column);

  apply_declaration(
    &mut style,
    &decl("grid-auto-flow", PropertyValue::Keyword("dense".into())),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_auto_flow, GridAutoFlow::RowDense);

  apply_declaration(
    &mut style,
    &decl(
      "grid-auto-flow",
      PropertyValue::Keyword("column/*comment*/dense".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_auto_flow, GridAutoFlow::ColumnDense);
}

#[test]
fn grid_auto_flow_ignores_invalid_values() {
  let mut style = ComputedStyle::default();
  let base = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid-auto-flow", PropertyValue::Keyword("column".into())),
    &base,
    16.0,
    16.0,
  );
  let expected = style.grid_auto_flow;

  // Invalid: multiple primary keywords.
  apply_declaration(
    &mut style,
    &decl("grid-auto-flow", PropertyValue::Keyword("row column".into())),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_auto_flow, expected);

  // Invalid: duplicate `dense`.
  apply_declaration(
    &mut style,
    &decl("grid-auto-flow", PropertyValue::Keyword("dense dense".into())),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_auto_flow, expected);

  // Invalid: unknown identifier.
  apply_declaration(
    &mut style,
    &decl("grid-auto-flow", PropertyValue::Keyword("wat".into())),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_auto_flow, expected);
}

