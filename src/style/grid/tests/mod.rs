use super::*;

mod grid_auto_flow_tokenization_test;
mod grid_line_case_insensitive_test;
mod grid_shorthand_auto_flow_detection_test;
mod grid_shorthand_case_insensitive_test;
mod subgrid_line_names_test;
mod subgrid_line_names_strict_test;

#[test]
fn grid_parsers_do_not_trim_non_ascii_whitespace() {
  let nbsp = "\u{00A0}";
  assert!(parse_grid_auto_flow_value("row").is_some());
  assert!(
    parse_grid_auto_flow_value(&format!("{nbsp}row")).is_none(),
    "NBSP must not be treated as CSS whitespace when parsing grid-auto-flow"
  );

  assert!(parse_grid_template_areas("\"a\"").is_some());
  assert!(
    parse_grid_template_areas(&format!("{nbsp}\"a\"")).is_none(),
    "NBSP must not be treated as CSS whitespace when parsing grid-template-areas"
  );
}

#[test]
fn parses_minmax_and_content_keywords() {
  let (tracks, _, _) = parse_grid_tracks_with_names("minmax(10px, 1fr) max-content min-content");
  assert_eq!(tracks.len(), 3);
  match &tracks[0] {
    GridTrack::MinMax(min, max) => {
      assert!(matches!(**min, GridTrack::Length(_)));
      assert!(matches!(**max, GridTrack::Fr(_)));
    }
    other => panic!("expected minmax track, got {:?}", other),
  }
  assert!(matches!(tracks[1], GridTrack::MaxContent));
  assert!(matches!(tracks[2], GridTrack::MinContent));
}

#[test]
fn ms_grid_track_repeat_syntax_expands() {
  assert_eq!(
    normalize_ms_grid_track_list("(1fr)[2]").as_deref(),
    Some("1fr 1fr")
  );
  assert_eq!(
    normalize_ms_grid_track_list("(1fr 32px)[2]").as_deref(),
    Some("1fr 32px 1fr 32px")
  );
  assert_eq!(
    normalize_ms_grid_track_list("1fr (32px)[3] 2fr").as_deref(),
    Some("1fr 32px 32px 32px 2fr")
  );
}

#[test]
fn parses_fit_content_and_percentages() {
  let (tracks, _, _) = parse_grid_tracks_with_names("fit-content(50%) 25%");
  assert_eq!(tracks.len(), 2);
  match &tracks[0] {
    GridTrack::FitContent(len) => assert_eq!(len.unit, crate::style::values::LengthUnit::Percent),
    other => panic!("expected fit-content track, got {:?}", other),
  }
  match &tracks[1] {
    GridTrack::Length(len) => assert_eq!(len.unit, crate::style::values::LengthUnit::Percent),
    other => panic!("expected percent length, got {:?}", other),
  }
}

#[test]
fn parses_repeat_with_line_names() {
  let (tracks, names, line_names) =
    parse_grid_tracks_with_names("[a] repeat(2, [b] 10px [c] minmax(0, 1fr)) [d]");
  assert_eq!(tracks.len(), 4);
  assert_eq!(names.get("a"), Some(&vec![0]));
  assert_eq!(names.get("b"), Some(&vec![0, 2]));
  assert_eq!(names.get("c"), Some(&vec![1, 3]));
  assert_eq!(names.get("d"), Some(&vec![4]));
  assert_eq!(line_names.len(), 5);
  assert!(line_names[0].contains(&"a".to_string()));
  assert!(line_names[0].contains(&"b".to_string())); // first repeat merges into current line
  assert!(line_names[1].contains(&"c".to_string()));
  assert!(line_names[2].contains(&"b".to_string()));
}

#[test]
fn parses_auto_fit_and_fill_repeat() {
  let (tracks_fit, names_fit, _) = parse_grid_tracks_with_names("repeat(auto-fit, 100px 1fr)");
  assert_eq!(tracks_fit.len(), 1);
  assert!(matches!(tracks_fit[0], GridTrack::RepeatAutoFit { .. }));
  assert!(names_fit.is_empty());

  let (tracks_fill, names_fill, _) = parse_grid_tracks_with_names("repeat(auto-fill, minmax(0, 1fr))");
  assert_eq!(tracks_fill.len(), 1);
  assert!(matches!(tracks_fill[0], GridTrack::RepeatAutoFill { .. }));
  assert!(names_fill.is_empty());
}

#[test]
fn auto_fit_repeat_keeps_named_lines() {
  let (_tracks, names, line_names) =
    parse_grid_tracks_with_names("repeat(auto-fit, [col-start] 10px [col-end])");
  assert_eq!(names.get("col-start"), Some(&vec![0]));
  assert_eq!(names.get("col-end"), Some(&vec![1]));
  assert!(line_names[0].contains(&"col-start".to_string()));
  assert!(line_names[1].contains(&"col-end".to_string()));
  assert_eq!(parse_grid_line("col-start", &names), 1);
  assert_eq!(parse_grid_line("col-end", &names), 2);
}

#[test]
fn auto_fill_repeat_keeps_named_lines() {
  let (_tracks, names, line_names) =
    parse_grid_tracks_with_names("repeat(auto-fill, [a] 20px [b c])");
  assert_eq!(names.get("a"), Some(&vec![0]));
  assert_eq!(names.get("b"), Some(&vec![1]));
  assert_eq!(names.get("c"), Some(&vec![1]));
  assert!(line_names[0].contains(&"a".to_string()));
  assert!(line_names[1].contains(&"b".to_string()));
  assert!(line_names[1].contains(&"c".to_string()));
  assert_eq!(parse_grid_line("a", &names), 1);
  assert_eq!(parse_grid_line("b", &names), 2);
  assert_eq!(parse_grid_line("c", &names), 2);
}

#[test]
fn finalize_leaves_named_tokens_for_layout() {
  let mut style = ComputedStyle::default();
  style.grid_column_raw = Some("foo / span 2".to_string());
  finalize_grid_placement(&mut style);
  assert_eq!(style.grid_column_start, 0);
  assert_eq!(style.grid_column_end, 0);
}

#[test]
fn finalize_skips_auto_repeat_resolution() {
  let mut style = ComputedStyle::default();
  style.grid_template_columns = vec![GridTrack::RepeatAutoFit {
    tracks: vec![GridTrack::Length(Length::px(50.0))],
    line_names: vec![vec!["a".into()], vec!["b".into()]],
  }];
  style.grid_column_raw = Some("1 / 2".to_string());
  finalize_grid_placement(&mut style);
  // Auto-repeat present: leave resolution to layout
  assert_eq!(style.grid_column_start, 0);
  assert_eq!(style.grid_column_end, 0);
  assert!(style.grid_column_raw.is_some());
}

#[test]
fn parses_grid_template_areas_rectangles() {
  let areas = parse_grid_template_areas("\"a a\" \"b .\"").expect("should parse");
  assert_eq!(areas.len(), 2);
  assert_eq!(areas[0].len(), 2);
  assert_eq!(areas[1].len(), 2);
  assert_eq!(areas[0][0], Some("a".into()));
  assert_eq!(areas[1][0], Some("b".into()));
  assert_eq!(areas[1][1], None);
}

#[test]
fn rejects_mismatched_columns_or_non_rectangles() {
  assert!(parse_grid_template_areas("\"a\" \"a a\"").is_none());
  // Non-rectangular area usage of "a"
  assert!(parse_grid_template_areas("\"a b\" \"a a\"").is_none());
}

#[test]
fn grid_template_areas_populates_tracks_when_empty() {
  let mut styles = ComputedStyle::default();
  let value = PropertyValue::Keyword("\"a b\" \"a b\"".into());
  match &value {
    PropertyValue::Keyword(kw) | PropertyValue::String(kw) => {
      if let Some(areas) = parse_grid_template_areas(kw) {
        let row_count = areas.len();
        let col_count = areas.first().map(|r| r.len()).unwrap_or(0);
        if col_count != 0 {
          styles.grid_template_areas = areas;
          if styles.grid_template_columns.is_empty() {
            styles.grid_template_columns = vec![GridTrack::Auto; col_count];
          }
          if styles.grid_template_rows.is_empty() {
            styles.grid_template_rows = vec![GridTrack::Auto; row_count];
          }
        }
      }
    }
    _ => {}
  }
  assert_eq!(styles.grid_template_columns.len(), 2);
  assert_eq!(styles.grid_template_rows.len(), 2);
}

#[test]
fn grid_template_shorthand_tracks_only() {
  let parsed = parse_grid_template_shorthand("100px auto / 1fr 2fr").expect("should parse");
  let (rows, _) = parsed.row_tracks.expect("rows");
  let (cols, _) = parsed.column_tracks.expect("cols");
  assert_eq!(rows.len(), 2);
  assert!(matches!(rows[0], GridTrack::Length(_)));
  assert!(matches!(rows[1], GridTrack::Auto));
  assert_eq!(cols.len(), 2);
  assert!(matches!(cols[0], GridTrack::Fr(_)));
}

#[test]
fn grid_template_shorthand_areas_with_sizes_and_cols() {
  let parsed =
    parse_grid_template_shorthand("\"a b\" 40px \"c d\" 50px / 20px 30px").expect("should parse");
  let areas = parsed.areas.expect("areas");
  assert_eq!(areas.len(), 2);
  assert_eq!(areas[0][0], Some("a".into()));
  let (rows, _) = parsed.row_tracks.expect("rows");
  assert_eq!(rows.len(), 2);
  assert!(matches!(rows[0], GridTrack::Length(_)));
  let (cols, _) = parsed.column_tracks.expect("cols");
  assert_eq!(cols.len(), 2);
  assert!(matches!(cols[0], GridTrack::Length(_)));
}

#[test]
fn grid_template_shorthand_areas_single_quotes() {
  let parsed = parse_grid_template_shorthand("'header' 'scroller' 'footer'/minmax(0, 1fr)")
    .expect("should parse");
  let areas = parsed.areas.expect("areas");
  assert_eq!(areas.len(), 3);
  assert_eq!(areas[0].len(), 1);
  assert_eq!(areas[0][0], Some("header".into()));
  assert_eq!(areas[1][0], Some("scroller".into()));
  assert_eq!(areas[2][0], Some("footer".into()));

  let (rows, _) = parsed.row_tracks.expect("rows");
  assert_eq!(rows.len(), 3);
  assert!(rows.iter().all(|t| matches!(t, GridTrack::Auto)));

  let (cols, _) = parsed.column_tracks.expect("cols");
  assert_eq!(cols.len(), 1);
  match &cols[0] {
    GridTrack::MinMax(min, max) => {
      assert!(
        matches!(**min, GridTrack::Length(len) if len.unit == crate::style::values::LengthUnit::Px && len.value == 0.0),
        "expected min track to be 0px, got {:?}",
        min
      );
      assert!(
        matches!(**max, GridTrack::Fr(fr) if fr == 1.0),
        "expected max track to be 1fr, got {:?}",
        max
      );
    }
    other => panic!("expected minmax track, got {:?}", other),
  }
}

#[test]
fn grid_template_shorthand_invalid_without_cols() {
  assert!(parse_grid_template_shorthand("100px auto").is_none());
}

#[test]
fn grid_template_shorthand_none_resets() {
  let parsed = parse_grid_template_shorthand("none").expect("should parse");
  assert!(parsed.areas.as_ref().unwrap().is_empty());
  assert!(parsed.row_tracks.as_ref().unwrap().0.is_empty());
  assert!(parsed.column_tracks.as_ref().unwrap().0.is_empty());
}

#[test]
fn parse_track_list_does_not_panic_on_unicode_prefix() {
  let parsed = parse_track_list("€€€repeat(2, 1fr)");
  assert!(parsed.tracks.is_empty());
}

#[test]
fn parses_subgrid_repeat_line_names() {
  let parsed = parse_subgrid_line_names("subgrid repeat(2, [a])").expect("should parse");
  assert_eq!(
    parsed,
    vec![vec!["a".to_string()], vec!["a".to_string()]]
  );
}

#[test]
fn parses_subgrid_repeat_line_names_mixed() {
  let parsed =
    parse_subgrid_line_names("subgrid [start] repeat(2, [mid]) [end]").expect("should parse");
  assert_eq!(
    parsed,
    vec![
      vec!["start".to_string()],
      vec!["mid".to_string()],
      vec!["mid".to_string()],
      vec!["end".to_string()]
    ]
  );
}
