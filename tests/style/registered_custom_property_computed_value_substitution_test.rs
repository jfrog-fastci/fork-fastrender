use fastrender::css::parser::parse_stylesheet;
use fastrender::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use fastrender::style::cascade::apply_styles;
use fastrender::style::color::Rgba;
use fastrender::style::properties::DEFAULT_VIEWPORT;
use fastrender::style::values::Length;

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
fn registered_custom_property_length_is_computed_in_declaration_context() {
  let css = r#"
    @property --len {
      syntax: "<length>";
      inherits: true;
      initial-value: 0px;
    }
    #root { font-size: 10px; --len: 1em; }
    #child { font-size: 20px; margin-left: var(--len); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child");

  assert_eq!(child.styles.margin_left, Some(Length::px(10.0)));
}

#[test]
fn registered_custom_property_length_substitutes_computed_value_on_same_element() {
  let css = r#"
    @property --len {
      syntax: "<length>";
      inherits: true;
      initial-value: 0px;
    }
    #root { font-size: 10px; --len: 1em; margin-left: var(--len); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);

  assert_eq!(styled.styles.margin_left, Some(Length::px(10.0)));
}

#[test]
fn registered_custom_property_color_resolves_current_color_at_declaration_site() {
  let css = r#"
    @property --c {
      syntax: "<color>";
      inherits: true;
      initial-value: rgb(0 0 0);
    }
    #root { color: rgb(255 0 0); --c: currentColor; }
    #child { color: rgb(0 0 255); background-color: var(--c); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child");

  assert_eq!(child.styles.background_color, Rgba::rgb(255, 0, 0));
}

#[test]
fn unregistered_custom_property_substitutes_tokens_in_use_site_context() {
  let css = r#"
    #root { font-size: 10px; --len: 1em; }
    #child { font-size: 20px; margin-left: var(--len); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child");

  let len = child.styles.margin_left.expect("computed margin-left");
  let resolved_px = len
    .resolve_with_context(
      None,
      DEFAULT_VIEWPORT.width,
      DEFAULT_VIEWPORT.height,
      child.styles.font_size,
      child.styles.root_font_size,
    )
    .expect("resolved margin-left");
  assert!((resolved_px - 20.0).abs() < 1e-6, "expected 20px, got {resolved_px}");
}
