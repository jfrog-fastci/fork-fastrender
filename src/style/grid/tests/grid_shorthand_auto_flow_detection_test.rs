use crate::css::properties::parse_property_value;
use crate::css::types::Declaration;
use crate::style::properties::apply_declaration;
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
fn grid_shorthand_auto_flow_detection_ignores_quoted_area_strings() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("grid", r#""auto-flow" 10px / 20px"#),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(
    styles.grid_template_areas,
    vec![vec![Some("auto-flow".to_string())]]
  );
  assert_eq!(
    styles.grid_template_rows,
    vec![GridTrack::Length(Length::px(10.0))]
  );
  assert_eq!(
    styles.grid_template_columns,
    vec![GridTrack::Length(Length::px(20.0))]
  );
}

