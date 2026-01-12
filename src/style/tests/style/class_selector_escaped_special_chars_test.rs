use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use fastrender::style::cascade::apply_styles;
use fastrender::LengthUnit;

#[test]
fn class_selector_can_match_brackets_and_parens_via_css_escapes() {
  // reddit.com's CSS (and many Tailwind-derived stylesheets) generates class selectors that
  // include characters like `[` and `(`, which must be escaped in CSS source.
  //
  // Example: `.px-\[var\(--rem14\)\] { ... }` should match `class="px-[var(--rem14)]"`.
  let css = r#"
    .px-\[var\(--rem14\)\] { padding-left: 10px; }
  "#;
  let sheet = parse_stylesheet(css).expect("parse stylesheet");

  let dom = DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![
        ("id".to_string(), "target".to_string()),
        ("class".to_string(), "px-[var(--rem14)]".to_string()),
      ],
    },
    children: vec![],
  };
  let styled = apply_styles(&dom, &sheet);

  assert_eq!(styled.styles.padding_left.value, 10.0);
  assert_eq!(styled.styles.padding_left.unit, LengthUnit::Px);
}

#[test]
fn not_pseudo_class_can_reference_escaped_backslash_class() {
  // Airbnb (and some other large sites) use `:not(.\\)` as a specificity hack.
  //
  // `.\\` is a class selector for the literal "\" class name. Since elements almost never have
  // that class, `:not(.\\)` is effectively always true, but it increases selector specificity.
  //
  // We need to parse the escaped class name correctly so rules like:
  //   .atm_mk_stnw88__320uii:not(.\\) { position: absolute; }
  // are not dropped as invalid.
  let css = r#"
    .a:not(.\\) { padding-left: 10px; }
  "#;
  let sheet = parse_stylesheet(css).expect("parse stylesheet");

  let dom_without_backslash_class = DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![("class".to_string(), "a".to_string())],
    },
    children: vec![],
  };
  let styled_without = apply_styles(&dom_without_backslash_class, &sheet);
  assert_eq!(styled_without.styles.padding_left.value, 10.0);
  assert_eq!(styled_without.styles.padding_left.unit, LengthUnit::Px);

  let dom_with_backslash_class = DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      // Two classes: "a" and "\".
      attributes: vec![("class".to_string(), r#"a \"#.to_string())],
    },
    children: vec![],
  };
  let styled_with = apply_styles(&dom_with_backslash_class, &sheet);
  assert_eq!(styled_with.styles.padding_left.value, 0.0);
  assert_eq!(styled_with.styles.padding_left.unit, LengthUnit::Px);
}
