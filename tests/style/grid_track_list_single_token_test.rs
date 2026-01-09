use fastrender::css::properties::parse_property_value;
use fastrender::css::types::Declaration;
use fastrender::css::types::PropertyValue;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::GridTrack;
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
fn grid_track_list_properties_preserve_single_token_values_for_downstream_parsing() {
  // These properties are parsed by `style::grid` later. When the value is a single token (e.g.
  // `100%`), keep it as a raw Keyword string so the grid parser sees a track list, rather than
  // eagerly parsing it as `<length>` and dropping it in `apply_declaration`.
  let parsed = parse_property_value("grid-template-columns", "100%").expect("parse");
  assert!(
    matches!(parsed, PropertyValue::Keyword(ref kw) if kw == "100%"),
    "expected Keyword(\"100%\") but got {parsed:?}"
  );

  let mut styles = ComputedStyle::default();
  apply_declaration(
    &mut styles,
    &decl("grid-template-columns", "100%"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.grid_template_columns.len(), 1);
  assert!(
    matches!(styles.grid_template_columns[0], GridTrack::Length(ref len) if len.unit == fastrender::style::values::LengthUnit::Percent),
    "expected a percent track, got {:?}",
    styles.grid_template_columns[0]
  );

  // Same issue applies to implicit track sizing properties.
  let parsed = parse_property_value("grid-auto-columns", "100%").expect("parse");
  assert!(
    matches!(parsed, PropertyValue::Keyword(ref kw) if kw == "100%"),
    "expected Keyword(\"100%\") but got {parsed:?}"
  );

  let mut styles = ComputedStyle::default();
  apply_declaration(
    &mut styles,
    &decl("grid-auto-columns", "100%"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.grid_auto_columns.len(), 1);
  assert!(
    matches!(styles.grid_auto_columns[0], GridTrack::Length(ref len) if len.unit == fastrender::style::values::LengthUnit::Percent),
    "expected a percent track, got {:?}",
    styles.grid_auto_columns[0]
  );
}

