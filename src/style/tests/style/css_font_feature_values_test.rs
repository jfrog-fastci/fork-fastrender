use cssparser::ToCss;

use crate::css::parser::parse_stylesheet;
use crate::css::types::StyleRule;
use crate::style::media::MediaContext;

fn selector_strings(rule: &StyleRule) -> Vec<String> {
  rule
    .selectors
    .slice()
    .iter()
    .map(|s| s.to_css_string())
    .collect()
}

#[test]
fn font_feature_values_at_rule_does_not_break_stylesheet_parsing() {
  // The `@font-feature-values` rule should either be parsed or ignored, but it must not prevent
  // subsequent style rules from being collected.
  let css = r#"
    @font-feature-values Foo {
      @styleset { nice-style: 1; }
    }
    .a { color: red; }
  "#;
  let sheet = parse_stylesheet(css).expect("stylesheet");
  let collected = sheet.collect_style_rules(&MediaContext::default());
  assert!(
    collected
      .iter()
      .any(|r| selector_strings(r.rule) == vec![".a".to_string()]),
    "expected `.a` style rule to be collected even with @font-feature-values present"
  );
}
