use fastrender::css::properties::parse_property_value;
use fastrender::css::types::Declaration;
use fastrender::css::types::PropertyValue;
use fastrender::style::properties::apply_declaration;
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
fn grid_template_areas_single_row_preserves_quotes_for_downstream_parsing() {
  // `grid-template-areas` expects a list of quoted strings where the quotes are semantically
  // significant (they delimit the area matrix rows). Preserve the authored token stream so the
  // grid parser can see the quotes.
  let parsed = parse_property_value("grid-template-areas", "'a b'").expect("parse");
  assert!(
    matches!(parsed, PropertyValue::Keyword(ref kw) if kw == "'a b'"),
    "expected Keyword(\"'a b'\") but got {parsed:?}"
  );

  let mut styles = ComputedStyle::default();
  apply_declaration(
    &mut styles,
    &decl("grid-template-areas", "'a b'"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(styles.grid_template_areas.len(), 1);
  assert_eq!(styles.grid_template_areas[0].len(), 2);
  assert_eq!(styles.grid_template_areas[0][0].as_deref(), Some("a"));
  assert_eq!(styles.grid_template_areas[0][1].as_deref(), Some("b"));
}

