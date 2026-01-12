use fastrender::css::parser::parse_stylesheet;
use fastrender::css::properties::parse_length;
use fastrender::dom::{DomNode, DomNodeType, HTML_NAMESPACE};
use fastrender::style::cascade::apply_styles;
use fastrender::style::color::Rgba;
use fastrender::style::properties::DEFAULT_VIEWPORT;
use fastrender::style::types::{LengthOrNumber, StrokeDasharray};
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
fn registered_custom_property_length_computes_at_declaration_site() {
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
  let child = styled.children.first().expect("child styles");

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
fn registered_custom_property_length_initial_value_supports_cap() {
  let css = r#"
    @property --len {
      syntax: "<length>";
      inherits: false;
      initial-value: 1cap;
    }
    #root { font-size: 10px; margin-left: var(--len); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);

  // 1cap uses the deterministic fallback: 0.7 * font-size.
  assert_eq!(styled.styles.margin_left, Some(Length::px(7.0)));
}

#[test]
fn registered_custom_property_length_max_is_not_flattened_across_arguments() {
  let css = r#"
    @property --len {
      syntax: "<length>";
      inherits: true;
      initial-value: 0px;
    }
    #root { font-size: 10px; --len: max(1em, 5px); }
    #child { font-size: 20px; margin-left: var(--len); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child styles");

  assert_eq!(child.styles.margin_left, Some(Length::px(10.0)));
}

#[test]
fn registered_custom_property_length_percentage_max_preserves_percent_terms() {
  let css = r#"
    @property --len {
      syntax: "<length-percentage>";
      inherits: true;
      initial-value: 0px;
    }
    #root { font-size: 10px; --len: max(50%, 1em); }
    #child { font-size: 20px; margin-left: var(--len); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child styles");

  let expected = parse_length("max(50%, 10px)").expect("parse expected max()");
  assert_eq!(child.styles.margin_left, Some(expected));
}

#[test]
fn registered_custom_property_length_vi_uses_writing_mode_inline_axis() {
  let css = r#"
    @property --len {
      syntax: "<length>";
      inherits: true;
      initial-value: 0px;
    }
    #root { writing-mode: vertical-rl; --len: 100vi; }
    #child { margin-left: var(--len); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child styles");

  assert_eq!(
    child.styles.margin_left,
    Some(Length::px(DEFAULT_VIEWPORT.height))
  );
}

#[test]
fn registered_custom_property_length_vb_uses_writing_mode_block_axis() {
  let css = r#"
    @property --len {
      syntax: "<length>";
      inherits: true;
      initial-value: 0px;
    }
    #root { writing-mode: vertical-rl; --len: 100vb; }
    #child { margin-left: var(--len); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child styles");

  assert_eq!(
    child.styles.margin_left,
    Some(Length::px(DEFAULT_VIEWPORT.width))
  );
}

#[test]
fn registered_custom_property_color_currentcolor_computes_at_declaration_site() {
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
  let child = styled.children.first().expect("child styles");

  assert_eq!(child.styles.background_color, Rgba::rgb(255, 0, 0));
}

#[test]
fn registered_custom_property_list_values_compute_at_declaration_site() {
  let css = r#"
    @property --xs {
      syntax: "<length>#";
      inherits: true;
      initial-value: 0px;
    }
    #root { font-size: 10px; --xs: 1em, 2em; }
    #child { font-size: 20px; stroke-dasharray: var(--xs); }
  "#;
  let sheet = parse_stylesheet(css).unwrap();
  let dom = dom_with_child();
  let styled = apply_styles(&dom, &sheet);
  let child = styled.children.first().expect("child styles");

  match &child.styles.svg_stroke_dasharray {
    Some(StrokeDasharray::Values(values)) => {
      assert_eq!(
        values.as_ref(),
        &[
          LengthOrNumber::Length(Length::px(10.0)),
          LengthOrNumber::Length(Length::px(20.0))
        ]
      );
    }
    other => panic!("expected computed stroke-dasharray, got {other:?}"),
  }
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
  let child = styled.children.first().expect("child styles");

  // Unregistered custom properties substitute raw tokens. `1em` is therefore interpreted against
  // the child's font-size, rather than being precomputed against the parent's font-size.
  assert_eq!(child.styles.margin_left, Some(Length::em(1.0)));
}
