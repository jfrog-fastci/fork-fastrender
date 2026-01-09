use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use fastrender::style::cascade::apply_styles;
use fastrender::style::color::Rgba;
use fastrender::style::values::{
  CustomPropertyListSeparator, CustomPropertySyntax, CustomPropertyTypedValue, Length,
  LengthUnit,
};

fn dom_with_child() -> DomNode {
  DomNode {
    node_type: DomNodeType::Element {
      tag_name: "div".to_string(),
      namespace: HTML_NAMESPACE.to_string(),
      attributes: vec![("id".to_string(), "root".to_string())],
    },
    children: vec![DomNode {
      node_type: DomNodeType::Element {
        tag_name: "span".to_string(),
        namespace: HTML_NAMESPACE.to_string(),
        attributes: vec![("id".to_string(), "child".to_string())],
      },
      children: vec![],
    }],
  }
}

#[test]
fn registered_custom_property_union_syntax_accepts_length_and_color() {
  let css = r#"
    @property --x {
      syntax: "<length> | <color>";
      inherits: true;
      initial-value: 0px;
    }
    #root { --x: 10px; }
    #child { --x: red; }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child");

  let rule = styled
    .styles
    .custom_property_registry
    .get("--x")
    .expect("registered property");
  match &rule.syntax {
    CustomPropertySyntax::Union(members) => {
      assert_eq!(
        members.as_ref(),
        &[CustomPropertySyntax::Length, CustomPropertySyntax::Color]
      );
    }
    other => panic!("expected union syntax, got {other:?}"),
  }

  let parent_value = styled
    .styles
    .custom_properties
    .get("--x")
    .expect("root value");
  assert_eq!(
    parent_value.typed,
    Some(CustomPropertyTypedValue::Length(Length::px(10.0)))
  );

  let child_value = child.styles.custom_properties.get("--x").expect("child value");
  match &child_value.typed {
    Some(CustomPropertyTypedValue::Color(color)) => {
      assert_eq!(color.to_rgba(Rgba::BLACK), Rgba::RED);
    }
    other => panic!("expected typed color for child, got {other:?}"),
  }
}

#[test]
fn invalid_union_custom_property_value_falls_back_to_initial_value() {
  let css = r#"
    @property --x {
      syntax: "<length> | <color>";
      inherits: true;
      initial-value: 0px;
    }
    #root { --x: 10px; }
    #child { --x: foo; }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child");

  let child_value = child.styles.custom_properties.get("--x").expect("child value");
  match &child_value.typed {
    Some(CustomPropertyTypedValue::Length(len)) => {
      assert_eq!(len.value, 0.0);
      assert_eq!(len.unit, LengthUnit::Px);
    }
    other => panic!("expected initial length on invalid value, got {other:?}"),
  }
}

#[test]
fn union_custom_property_inherits_descriptor_controls_inheritance() {
  let css = r#"
    @property --inherit {
      syntax: "<length> | <color>";
      inherits: true;
      initial-value: 0px;
    }
    @property --noinherit {
      syntax: "<length> | <color>";
      inherits: false;
      initial-value: 0px;
    }
    #root { --inherit: red; --noinherit: red; }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child");

  let inherited = child
    .styles
    .custom_properties
    .get("--inherit")
    .expect("inherited value");
  match &inherited.typed {
    Some(CustomPropertyTypedValue::Color(color)) => {
      assert_eq!(color.to_rgba(Rgba::BLACK), Rgba::RED);
    }
    other => panic!("expected inherited typed color, got {other:?}"),
  }

  let non_inherited = child
    .styles
    .custom_properties
    .get("--noinherit")
    .expect("non-inherited value");
  match &non_inherited.typed {
    Some(CustomPropertyTypedValue::Length(len)) => {
      assert_eq!(len.value, 0.0);
      assert_eq!(len.unit, LengthUnit::Px);
    }
    other => panic!("expected initial length for non-inheriting property, got {other:?}"),
  }
}

#[test]
fn registered_custom_property_list_syntax_parses_comma_separated_values() {
  let css = r#"
    @property --xs {
      syntax: "<length>#";
      inherits: false;
      initial-value: 0px;
    }
    #root { --xs: 10px, 20px; }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);

  let value = styled
    .styles
    .custom_properties
    .get("--xs")
    .expect("computed property");
  match &value.typed {
    Some(CustomPropertyTypedValue::List { separator, items }) => {
      assert_eq!(*separator, CustomPropertyListSeparator::Comma);
      assert_eq!(
        items,
        &vec![
          CustomPropertyTypedValue::Length(Length::px(10.0)),
          CustomPropertyTypedValue::Length(Length::px(20.0)),
        ]
      );
    }
    other => panic!("expected typed list value, got {other:?}"),
  }
}

