use fastrender::css::types::Declaration;
use fastrender::css::types::PropertyValue;
use fastrender::style::properties::apply_declaration;
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
fn grid_shorthand_auto_flow_detection_ignores_bracketed_line_names() {
  let mut style = ComputedStyle::default();
  let base = ComputedStyle::default();

  // The token `auto-flow` inside `[ ... ]` is a line name, not the `grid` auto-flow shorthand
  // keyword. Ensure we parse as the grid-template form.
  apply_declaration(
    &mut style,
    &decl("grid", PropertyValue::Keyword("[auto-flow] 1fr / 2fr".into())),
    &base,
    16.0,
    16.0,
  );

  assert_eq!(style.grid_template_rows.len(), 1);
  assert_eq!(style.grid_template_columns.len(), 1);
  assert!(matches!(style.grid_template_rows[0], GridTrack::Fr(_)));
  assert!(matches!(style.grid_template_columns[0], GridTrack::Fr(_)));
}

#[test]
fn grid_shorthand_auto_flow_detection_ignores_area_strings() {
  let mut style = ComputedStyle::default();
  let base = ComputedStyle::default();

  // `auto-flow` inside a template area string should not opt into the auto-flow shorthand form.
  apply_declaration(
    &mut style,
    &decl("grid", PropertyValue::Keyword("\"auto-flow\" / 1fr".into())),
    &base,
    16.0,
    16.0,
  );

  assert_eq!(style.grid_template_areas.len(), 1);
  assert_eq!(
    style.grid_template_areas[0][0].as_deref(),
    Some("auto-flow")
  );
  assert_eq!(style.grid_template_columns.len(), 1);
}

#[test]
fn grid_shorthand_auto_flow_detection_triggers_on_keyword_outside_strings() {
  let mut style = ComputedStyle::default();
  let base = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid", PropertyValue::Keyword("auto-flow 1fr / 2fr".into())),
    &base,
    16.0,
    16.0,
  );

  // Auto-flow form resets the explicit template track lists.
  assert!(style.grid_template_rows.is_empty());
  assert!(style.grid_template_columns.is_empty());

  assert_eq!(style.grid_auto_rows.len(), 1);
  assert_eq!(style.grid_auto_columns.len(), 1);
  assert!(matches!(style.grid_auto_rows[0], GridTrack::Fr(_)));
  assert!(matches!(style.grid_auto_columns[0], GridTrack::Fr(_)));
}

