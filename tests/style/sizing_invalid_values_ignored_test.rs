use fastrender::css::parser::parse_declarations;
use fastrender::style::properties::apply_declaration;
use fastrender::style::values::Length;
use fastrender::style::ComputedStyle;

#[test]
fn invalid_width_keyword_does_not_override_prior_valid_width() {
  let decls = parse_declarations("width: 10px; width: foo;");
  assert_eq!(decls.len(), 2);

  let parent = ComputedStyle::default();
  let mut styles = ComputedStyle::default();
  for decl in &decls {
    apply_declaration(&mut styles, decl, &parent, 16.0, 16.0);
  }

  assert_eq!(styles.width, Some(Length::px(10.0)));
  assert_eq!(styles.width_keyword, None);
}

#[test]
fn unitless_width_number_does_not_override_prior_valid_width() {
  let decls = parse_declarations("width: 10px; width: 1;");
  assert_eq!(decls.len(), 2);

  let parent = ComputedStyle::default();
  let mut styles = ComputedStyle::default();
  for decl in &decls {
    apply_declaration(&mut styles, decl, &parent, 16.0, 16.0);
  }

  assert_eq!(styles.width, Some(Length::px(10.0)));
  assert_eq!(styles.width_keyword, None);
}

#[test]
fn width_none_keyword_is_invalid_but_max_width_none_is_valid() {
  let parent = ComputedStyle::default();

  let mut styles = ComputedStyle::default();
  for decl in &parse_declarations("width: 10px; width: none;") {
    apply_declaration(&mut styles, decl, &parent, 16.0, 16.0);
  }
  assert_eq!(styles.width, Some(Length::px(10.0)));
  assert_eq!(styles.width_keyword, None);

  let mut styles = ComputedStyle::default();
  for decl in &parse_declarations("max-width: 10px; max-width: none;") {
    apply_declaration(&mut styles, decl, &parent, 16.0, 16.0);
  }
  assert_eq!(styles.max_width, None);
  assert_eq!(styles.max_width_keyword, None);
}

