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
fn grid_template_areas_create_line_names() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl(
      "grid-template-areas",
      PropertyValue::Keyword("\"a a\" \"b c\"".into()),
    ),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  // grid-template-areas should synthesize track sizing when absent
  assert_eq!(
    style.grid_template_columns,
    vec![GridTrack::Auto, GridTrack::Auto]
  );
  assert_eq!(
    style.grid_template_rows,
    vec![GridTrack::Auto, GridTrack::Auto]
  );
  assert_eq!(style.grid_column_line_names.len(), 3);
  assert_eq!(style.grid_row_line_names.len(), 3);

  // Area names define start/end line names on the corresponding boundaries
  assert!(style.grid_column_line_names[0].contains(&"a-start".to_string()));
  assert!(style.grid_column_line_names[0].contains(&"b-start".to_string()));
  assert!(style.grid_column_line_names[2].contains(&"a-end".to_string()));
  assert!(style.grid_column_line_names[2].contains(&"c-end".to_string()));

  assert!(style.grid_row_line_names[0].contains(&"a-start".to_string()));
  assert!(style.grid_row_line_names[1].contains(&"a-end".to_string()));
  assert!(style.grid_row_line_names[1].contains(&"b-start".to_string()));
  assert!(style.grid_row_line_names[2].contains(&"b-end".to_string()));
  assert!(style.grid_row_line_names[1].contains(&"c-start".to_string()));
  assert!(style.grid_row_line_names[2].contains(&"c-end".to_string()));
}

#[test]
fn grid_template_shorthand_synthesizes_area_line_names() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl(
      "grid-template",
      PropertyValue::Keyword("\"a a\" \"b c\" / 20px 30px".into()),
    ),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(style.grid_template_columns.len(), 2);
  assert_eq!(style.grid_template_rows.len(), 2);
  assert_eq!(style.grid_column_line_names.len(), 3);
  assert_eq!(style.grid_row_line_names.len(), 3);

  assert!(style.grid_column_line_names[0].contains(&"a-start".to_string()));
  assert!(style.grid_column_line_names[2].contains(&"a-end".to_string()));
  assert!(style.grid_row_line_names[1].contains(&"b-start".to_string()));
  assert!(style.grid_row_line_names[2].contains(&"b-end".to_string()));
}

#[test]
fn grid_shorthand_template_synthesizes_area_line_names() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid", PropertyValue::Keyword("\"x x\" \"y z\"".into())),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(style.grid_template_columns.len(), 2);
  assert_eq!(style.grid_template_rows.len(), 2);
  assert_eq!(style.grid_column_line_names.len(), 3);
  assert_eq!(style.grid_row_line_names.len(), 3);

  assert!(style.grid_column_line_names[0].contains(&"x-start".to_string()));
  assert!(style.grid_column_line_names[2].contains(&"x-end".to_string()));
  assert!(style.grid_row_line_names[1].contains(&"y-start".to_string()));
  assert!(style.grid_row_line_names[2].contains(&"y-end".to_string()));
}

#[test]
fn grid_template_columns_ignores_invalid_values() {
  let mut style = ComputedStyle::default();
  let base = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("10px".into()),
    ),
    &base,
    16.0,
    16.0,
  );

  assert!(!style.grid_template_columns.is_empty());
  let expected_tracks = style.grid_template_columns.clone();
  let expected_names = style.grid_column_names.clone();
  let expected_line_names = style.grid_column_line_names.clone();
  let expected_subgrid = style.grid_column_subgrid;
  let expected_subgrid_line_names = style.subgrid_column_line_names.clone();

  // Invalid: only line names without any track sizes.
  apply_declaration(
    &mut style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("[a]".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_template_columns, expected_tracks);
  assert_eq!(style.grid_column_names, expected_names);
  assert_eq!(style.grid_column_line_names, expected_line_names);
  assert_eq!(style.grid_column_subgrid, expected_subgrid);
  assert_eq!(style.subgrid_column_line_names, expected_subgrid_line_names);

  // Invalid: `subgrid` cannot be followed by track sizes.
  apply_declaration(
    &mut style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("subgrid 10px".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_template_columns, expected_tracks);
  assert_eq!(style.grid_column_names, expected_names);
  assert_eq!(style.grid_column_line_names, expected_line_names);
  assert_eq!(style.grid_column_subgrid, expected_subgrid);
  assert_eq!(style.subgrid_column_line_names, expected_subgrid_line_names);

  // `none` resets the track list.
  apply_declaration(
    &mut style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("none".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  assert!(style.grid_template_columns.is_empty());
  assert!(style.grid_column_names.is_empty());
  assert_eq!(style.grid_column_line_names, vec![Vec::<String>::new()]);
  assert!(!style.grid_column_subgrid);
  assert!(style.subgrid_column_line_names.is_empty());
}

#[test]
fn grid_template_rows_ignores_invalid_values() {
  let mut style = ComputedStyle::default();
  let base = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid-template-rows", PropertyValue::Keyword("10px".into())),
    &base,
    16.0,
    16.0,
  );

  assert!(!style.grid_template_rows.is_empty());
  let expected_tracks = style.grid_template_rows.clone();
  let expected_names = style.grid_row_names.clone();
  let expected_line_names = style.grid_row_line_names.clone();
  let expected_subgrid = style.grid_row_subgrid;
  let expected_subgrid_line_names = style.subgrid_row_line_names.clone();

  apply_declaration(
    &mut style,
    &decl("grid-template-rows", PropertyValue::Keyword("[a]".into())),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_template_rows, expected_tracks);
  assert_eq!(style.grid_row_names, expected_names);
  assert_eq!(style.grid_row_line_names, expected_line_names);
  assert_eq!(style.grid_row_subgrid, expected_subgrid);
  assert_eq!(style.subgrid_row_line_names, expected_subgrid_line_names);

  apply_declaration(
    &mut style,
    &decl(
      "grid-template-rows",
      PropertyValue::Keyword("subgrid 10px".into()),
    ),
    &base,
    16.0,
    16.0,
  );
  assert_eq!(style.grid_template_rows, expected_tracks);
  assert_eq!(style.grid_row_names, expected_names);
  assert_eq!(style.grid_row_line_names, expected_line_names);
  assert_eq!(style.grid_row_subgrid, expected_subgrid);
  assert_eq!(style.subgrid_row_line_names, expected_subgrid_line_names);

  apply_declaration(
    &mut style,
    &decl("grid-template-rows", PropertyValue::Keyword("none".into())),
    &base,
    16.0,
    16.0,
  );
  assert!(style.grid_template_rows.is_empty());
  assert!(style.grid_row_names.is_empty());
  assert_eq!(style.grid_row_line_names, vec![Vec::<String>::new()]);
  assert!(!style.grid_row_subgrid);
  assert!(style.subgrid_row_line_names.is_empty());
}

#[test]
fn grid_template_columns_subgrid_sets_line_names() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("subgrid [a] [b]".into()),
    ),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert!(style.grid_column_subgrid);
  assert_eq!(
    style.subgrid_column_line_names,
    vec![vec!["a".to_string()], vec!["b".to_string()]]
  );
}
