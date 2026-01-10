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

