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
fn grid_template_areas_parses_even_when_track_counts_differ() {
  let mut style = ComputedStyle::default();

  // Ensure `grid-template-areas` is accepted even when grid-template-columns has a different number
  // of tracks. Per spec, the explicit grid size is the max of the area matrix size and the sized
  // tracks count; extra tracks take their sizing from grid-auto-rows/columns.
  apply_declaration(
    &mut style,
    &decl(
      "grid-template-columns",
      PropertyValue::Keyword("auto".into()),
    ),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

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

  assert_eq!(style.grid_template_areas.len(), 2);
  assert_eq!(style.grid_template_areas[0].len(), 2);
  assert_eq!(style.grid_template_areas[0][0].as_deref(), Some("a"));
  assert_eq!(style.grid_template_areas[0][1].as_deref(), Some("a"));
  assert_eq!(style.grid_template_areas[1][0].as_deref(), Some("b"));
  assert_eq!(style.grid_template_areas[1][1].as_deref(), Some("c"));

  // `none` clears the template areas.
  apply_declaration(
    &mut style,
    &decl("grid-template-areas", PropertyValue::Keyword("none".into())),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );
  assert!(style.grid_template_areas.is_empty());
}

#[test]
fn grid_template_shorthand_sets_areas_and_tracks() {
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
  assert_eq!(style.grid_template_areas.len(), 2);
  assert_eq!(style.grid_template_areas[0].len(), 2);
}

#[test]
fn grid_shorthand_sets_template_areas() {
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
  assert_eq!(style.grid_template_areas.len(), 2);
  assert_eq!(style.grid_template_areas[0].len(), 2);
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

#[test]
fn grid_template_shorthand_supports_subgrid_columns_only() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid-template", PropertyValue::Keyword("auto / subgrid".into())),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(style.grid_template_rows, vec![GridTrack::Auto]);
  assert!(style.grid_template_columns.is_empty());
  assert!(style.grid_column_subgrid);
}

#[test]
fn grid_template_shorthand_supports_subgrid_rows_only() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid-template", PropertyValue::Keyword("subgrid / auto".into())),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(style.grid_template_columns, vec![GridTrack::Auto]);
  assert!(style.grid_template_rows.is_empty());
  assert!(style.grid_row_subgrid);
}

#[test]
fn grid_shorthand_supports_subgrid_columns_only() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid", PropertyValue::Keyword("auto / subgrid".into())),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(style.grid_template_rows, vec![GridTrack::Auto]);
  assert!(style.grid_template_columns.is_empty());
  assert!(style.grid_column_subgrid);
}

#[test]
fn grid_shorthand_supports_subgrid_rows_only() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl("grid", PropertyValue::Keyword("subgrid / auto".into())),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(style.grid_template_columns, vec![GridTrack::Auto]);
  assert!(style.grid_template_rows.is_empty());
  assert!(style.grid_row_subgrid);
}

#[test]
fn grid_template_shorthand_parses_subgrid_line_names() {
  let mut style = ComputedStyle::default();

  apply_declaration(
    &mut style,
    &decl(
      "grid-template",
      PropertyValue::Keyword("subgrid [a] [b] / auto".into()),
    ),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert!(style.grid_row_subgrid);
  assert!(style.grid_template_rows.is_empty());
  assert_eq!(
    style.subgrid_row_line_names,
    vec![vec!["a".to_string()], vec!["b".to_string()]]
  );
}
