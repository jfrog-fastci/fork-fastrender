use crate::css::parser::parse_stylesheet;
use crate::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use crate::style::cascade::apply_styles;
use crate::style::values::{CustomPropertySyntax, CustomPropertyTypedValue};

fn simple_div_dom() -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![],
    },
    children: vec![],
  }
}

#[test]
fn custom_property_registration_accepts_integer_initial_value() {
  let css = r#"
    @property --i {
      syntax: "<integer>";
      inherits: true;
      initial-value: 2;
    }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = simple_div_dom();
  let styled = apply_styles(&dom, &sheet);

  let rule = styled
    .styles
    .custom_property_registry
    .get("--i")
    .expect("registered property");
  assert_eq!(rule.syntax, CustomPropertySyntax::Integer);

  let value = styled
    .styles
    .custom_properties
    .get("--i")
    .expect("initial value");
  assert_eq!(value.typed, Some(CustomPropertyTypedValue::Integer(2)));
}

#[test]
fn custom_property_registration_rejects_invalid_integer_initial_value() {
  let css = r#"
    @property --bad {
      syntax: "<integer>";
      inherits: true;
      initial-value: 2.5;
    }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = simple_div_dom();
  let styled = apply_styles(&dom, &sheet);

  assert!(
    styled
      .styles
      .custom_property_registry
      .get("--bad")
      .is_none(),
    "invalid <integer> initial-value should cause @property registration to be ignored"
  );
}

#[test]
fn custom_property_registration_parses_time_initial_value_to_ms() {
  let css = r#"
    @property --t {
      syntax: "<time>";
      inherits: true;
      initial-value: 1.5s;
    }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = simple_div_dom();
  let styled = apply_styles(&dom, &sheet);

  let rule = styled
    .styles
    .custom_property_registry
    .get("--t")
    .expect("registered property");
  assert_eq!(rule.syntax, CustomPropertySyntax::Time);

  let value = styled
    .styles
    .custom_properties
    .get("--t")
    .expect("initial value");
  assert_eq!(value.typed, Some(CustomPropertyTypedValue::TimeMs(1500.0)));
  assert_eq!(value.value.trim(), "1500ms");
}

#[test]
fn custom_property_registration_parses_resolution_initial_value_to_dppx() {
  let css = r#"
    @property --r {
      syntax: "<resolution>";
      inherits: true;
      initial-value: 192dpi;
    }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = simple_div_dom();
  let styled = apply_styles(&dom, &sheet);

  let rule = styled
    .styles
    .custom_property_registry
    .get("--r")
    .expect("registered property");
  assert_eq!(rule.syntax, CustomPropertySyntax::Resolution);

  let value = styled
    .styles
    .custom_properties
    .get("--r")
    .expect("initial value");
  assert_eq!(
    value.typed,
    Some(CustomPropertyTypedValue::ResolutionDppx(2.0))
  );
  assert_eq!(value.value.trim(), "2dppx");
}
