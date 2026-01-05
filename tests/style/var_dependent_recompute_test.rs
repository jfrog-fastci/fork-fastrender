use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use fastrender::style::cascade::apply_styles;
use fastrender::style::properties::DEFAULT_VIEWPORT;
use fastrender::style::values::CustomPropertyValue;
use fastrender::style::ComputedStyle;
use fastrender::Length;

#[test]
fn recompute_updates_var_dependent_property_after_custom_property_change() {
  let css = r#"
    #el { --x: 0; width: calc(10px * var(--x)); }
  "#;

  let sheet = parse_stylesheet(css).unwrap();
  let dom = DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![("id".to_string(), "el".to_string())],
    },
    children: vec![],
  };
  let styled = apply_styles(&dom, &sheet);

  let mut style = (*styled.styles).clone();
  assert_eq!(style.width, Some(Length::px(0.0)));

  style
    .custom_properties
    .insert("--x".into(), CustomPropertyValue::new("1", None));
  style.recompute_var_dependent_properties(&ComputedStyle::default(), DEFAULT_VIEWPORT);

  assert_eq!(style.width, Some(Length::px(10.0)));
}
