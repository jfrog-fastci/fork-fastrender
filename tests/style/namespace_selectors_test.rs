use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node
    .node
    .get_attribute_ref("id")
    .is_some_and(|value| value.eq_ignore_ascii_case(id))
  {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn display(node: &StyledNode) -> String {
  node.styles.display.to_string()
}

#[test]
fn undeclared_prefix_invalidates_rule() {
  let html = r#"<svg><rect id="r"></rect></svg>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    rect { display: flex; }
    svg|rect { display: block; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let rect = find_by_id(&styled, "r").expect("rect");
  assert_eq!(display(rect), "flex");
}

#[test]
fn default_namespace_restricts_unprefixed_type_selectors() {
  let html = r#"<g id="html"></g><svg><g id="svg"></g></svg>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    @namespace url("http://www.w3.org/2000/svg");
    @namespace html url("http://www.w3.org/1999/xhtml");
    html|g { display: flex; }
    g { display: block; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let html_g = find_by_id(&styled, "html").expect("html g");
  let svg_g = find_by_id(&styled, "svg").expect("svg g");
  assert_eq!(display(html_g), "flex");
  assert_eq!(display(svg_g), "block");
}

#[test]
fn namespace_rule_after_style_rule_is_ignored() {
  let html = r#"<svg><rect id="r"></rect></svg>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    rect { display: flex; }
    @namespace svg url("http://www.w3.org/2000/svg");
    svg|rect { display: block; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let rect = find_by_id(&styled, "r").expect("rect");
  assert_eq!(display(rect), "flex");
}

#[test]
fn explicit_no_namespace_selector_does_not_match_html() {
  let html = r#"<div id="d"></div>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    div { display: block; }
    |div { display: none; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let div = find_by_id(&styled, "d").expect("div");
  assert_eq!(display(div), "block");
}
