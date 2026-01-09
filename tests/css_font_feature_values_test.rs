use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::{CssRule, FontFeatureValueType};

#[test]
fn font_feature_values_parses_mdn_minified_form() {
  let css = r#"@font-feature-values "Inter"{@styleset{disambiguation:2}}"#;
  let sheet = parse_stylesheet(css).expect("parse_stylesheet");
  let rule = sheet
    .rules
    .iter()
    .find_map(|rule| match rule {
      CssRule::FontFeatureValues(rule) => Some(rule),
      _ => None,
    })
    .expect("expected @font-feature-values rule");

  assert_eq!(rule.font_families, vec!["Inter"]);

  let styleset = rule
    .groups
    .get(&FontFeatureValueType::Styleset)
    .expect("expected @styleset group");
  assert_eq!(styleset.get("disambiguation"), Some(&vec![2u32]));
}

#[test]
fn font_feature_values_prelude_rejects_generic_families() {
  let css = r#"@font-feature-values serif { @styleset { a: 1; } }"#;
  let sheet = parse_stylesheet(css).expect("parse_stylesheet");
  assert!(
    !sheet
      .rules
      .iter()
      .any(|rule| matches!(rule, CssRule::FontFeatureValues(_))),
    "expected generic-family prelude to skip @font-feature-values rule"
  );
}

#[test]
fn font_feature_values_ignores_unknown_nested_at_rules() {
  let css = r#"@font-feature-values Foo { @unknown { a: 1; } @styleset { b: 2; } }"#;
  let sheet = parse_stylesheet(css).expect("parse_stylesheet");
  let rule = sheet
    .rules
    .iter()
    .find_map(|rule| match rule {
      CssRule::FontFeatureValues(rule) => Some(rule),
      _ => None,
    })
    .expect("expected @font-feature-values rule");

  assert_eq!(rule.font_families, vec!["Foo"]);

  let styleset = rule
    .groups
    .get(&FontFeatureValueType::Styleset)
    .expect("expected @styleset group");
  assert_eq!(styleset.get("b"), Some(&vec![2u32]));
}
