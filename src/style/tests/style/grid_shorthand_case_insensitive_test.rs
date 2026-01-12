use fastrender::css::types::Declaration;
use fastrender::css::types::PropertyValue;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::GridAutoFlow;
use fastrender::style::types::GridTrack;
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
fn grid_shorthand_auto_flow_keywords_are_case_insensitive() {
  let mut style = ComputedStyle::default();
  let base = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl(
      "grid",
      PropertyValue::Keyword("AUTO-FLOW CoLuMn DeNsE 10px / 20px".into()),
    ),
    &base,
    16.0,
    16.0,
  );

  assert_eq!(style.grid_auto_flow, GridAutoFlow::ColumnDense);
  assert_eq!(style.grid_auto_rows.len(), 1);
  assert_eq!(style.grid_auto_columns.len(), 1);
  assert!(matches!(style.grid_auto_rows[0], GridTrack::Length(_)));
  assert!(matches!(style.grid_auto_columns[0], GridTrack::Length(_)));
}

#[test]
fn grid_shorthand_none_is_case_insensitive() {
  let mut style = ComputedStyle::default();
  let base = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid", PropertyValue::Keyword("NoNe".into())),
    &base,
    16.0,
    16.0,
  );

  // `grid: none` resets the template + implicit track lists.
  assert!(style.grid_template_rows.is_empty());
  assert!(style.grid_template_columns.is_empty());
  assert!(style.grid_template_areas.is_empty());

  assert_eq!(style.grid_auto_flow, GridAutoFlow::Row);
}

