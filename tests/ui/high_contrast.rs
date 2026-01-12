use fastrender::ui::high_contrast::{parse_env_bool, parse_high_contrast_env, theme_tuning};

#[test]
fn high_contrast_env_parsing_truthy_and_falsey_values() {
  for v in [None, Some(""), Some("   "), Some("0"), Some("false"), Some("FALSE"), Some("no"), Some("off"), Some(" 0 "), Some(" off ")] {
    assert!(
      !parse_high_contrast_env(v),
      "expected {v:?} to be parsed as false"
    );
  }

  for v in [
    Some("1"),
    Some("true"),
    Some("TRUE"),
    Some("yes"),
    Some("on"),
    Some("anything"),
    Some(" 1 "),
  ] {
    assert!(
      parse_high_contrast_env(v),
      "expected {v:?} to be parsed as true"
    );
  }

  // `parse_env_bool` is the primitive used by the env parser; keep its trimming behaviour covered.
  assert!(!parse_env_bool("  "));
  assert!(!parse_env_bool("  false "));
  assert!(parse_env_bool("  true "));
}

#[test]
fn high_contrast_theme_tuning_increases_contrast_and_focus_strength() {
  let normal = theme_tuning(false);
  let high = theme_tuning(true);

  assert!(
    high.bg_stroke_width > normal.bg_stroke_width,
    "expected high-contrast bg_stroke_width to be stronger ({} > {})",
    high.bg_stroke_width,
    normal.bg_stroke_width
  );
  assert!(
    high.focus_stroke_width > normal.focus_stroke_width,
    "expected high-contrast focus_stroke_width to be stronger ({} > {})",
    high.focus_stroke_width,
    normal.focus_stroke_width
  );
  assert!(
    high.focus_stroke_alpha > normal.focus_stroke_alpha,
    "expected high-contrast focus_stroke_alpha to be higher ({} > {})",
    high.focus_stroke_alpha,
    normal.focus_stroke_alpha
  );
  assert!(
    high.selection_bg_alpha > normal.selection_bg_alpha,
    "expected high-contrast selection_bg_alpha to be higher ({} > {})",
    high.selection_bg_alpha,
    normal.selection_bg_alpha
  );
  assert!(
    high.hover_stroke_alpha > normal.hover_stroke_alpha,
    "expected high-contrast hover_stroke_alpha to be higher ({} > {})",
    high.hover_stroke_alpha,
    normal.hover_stroke_alpha
  );
}

