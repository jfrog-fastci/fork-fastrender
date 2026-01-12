use fastrender::css::properties::parse_property_value;
use fastrender::css::types::Declaration;
use fastrender::style::properties::apply_declaration;
use fastrender::style::types::{BackgroundSize, BackgroundSizeComponent, BackgroundSizeKeyword};
use fastrender::style::values::Length;
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
fn background_size_rejects_unknown_tokens() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-size", "10px 20px"),
    &parent,
    16.0,
    16.0,
  );
  let expected = styles.background_sizes[0];

  apply_declaration(
    &mut styles,
    &decl("background-size", "10px bogus"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.background_sizes[0], expected);
}

#[test]
fn background_size_rejects_too_many_components() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-size", "10px 20px"),
    &parent,
    16.0,
    16.0,
  );
  let expected = styles.background_sizes[0];

  apply_declaration(
    &mut styles,
    &decl("background-size", "10px 20px 30px"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.background_sizes[0], expected);
}

#[test]
fn mask_size_rejects_unknown_tokens() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("mask-size", "10px 20px"),
    &parent,
    16.0,
    16.0,
  );
  let expected = styles.mask_sizes[0];

  apply_declaration(
    &mut styles,
    &decl("mask-size", "10px bogus"),
    &parent,
    16.0,
    16.0,
  );
  assert_eq!(styles.mask_sizes[0], expected);
}

#[test]
fn background_shorthand_accepts_case_insensitive_size_keywords() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background", "url(a) center/COVER no-repeat"),
    &parent,
    16.0,
    16.0,
  );

  assert_eq!(
    styles.background_sizes[0],
    BackgroundSize::Keyword(BackgroundSizeKeyword::Cover)
  );
}

#[test]
fn mask_shorthand_accepts_case_insensitive_size_keywords() {
  let mut styles = ComputedStyle::default();
  let parent = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("mask", "url(a) center/CONTAIN no-repeat"),
    &parent,
    16.0,
    16.0,
  );

  assert_eq!(
    styles.mask_sizes[0],
    BackgroundSize::Keyword(BackgroundSizeKeyword::Contain)
  );
}

#[test]
fn background_size_still_parses_valid_components() {
  let mut styles = ComputedStyle::default();

  apply_declaration(
    &mut styles,
    &decl("background-size", "10px 20px"),
    &ComputedStyle::default(),
    16.0,
    16.0,
  );

  assert_eq!(
    styles.background_sizes[0],
    BackgroundSize::Explicit(
      BackgroundSizeComponent::Length(Length::px(10.0)),
      BackgroundSizeComponent::Length(Length::px(20.0))
    )
  );
}
