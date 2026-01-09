use fastrender::css::parser::parse_stylesheet;
use fastrender::css::types::CssRule;
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
fn namespace_prefixed_selectors_match_when_namespace_declared() {
  let html = r#"<svg><foreignObject id="svg"></foreignObject></svg>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    @namespace svg "http://www.w3.org/2000/svg";
    svg|foreignObject { display: none; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let fo = find_by_id(&styled, "svg").expect("foreignObject");
  assert_eq!(display(fo), "none");
}

#[test]
fn namespace_prefixed_selectors_do_not_match_without_declaration() {
  let html = r#"<svg><foreignObject id="svg"></foreignObject></svg>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"svg|foreignObject { display: none; }"#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let fo = find_by_id(&styled, "svg").expect("foreignObject");
  assert_ne!(display(fo), "none");
}

#[test]
fn default_namespace_applies_to_unprefixed_type_selectors() {
  let html = r#"
    <foreignObject id="html"></foreignObject>
    <svg><foreignObject id="svg"></foreignObject></svg>
  "#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    @namespace "http://www.w3.org/2000/svg";
    foreignObject { display: none; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let svg = find_by_id(&styled, "svg").expect("svg foreignObject");
  let html = find_by_id(&styled, "html").expect("html foreignObject");
  assert_eq!(display(svg), "none");
  assert_ne!(display(html), "none");
}

#[test]
fn namespace_rules_after_qualified_rules_are_ignored() {
  let html = r#"<svg><rect id="r"></rect></svg>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    rect { display: block; }
    @namespace svg "http://www.w3.org/2000/svg";
    svg|rect { display: none; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let rect = find_by_id(&styled, "r").expect("rect");
  assert_eq!(display(rect), "block");
}

#[test]
fn namespace_prefixed_selectors_match_mathml() {
  let html = r#"<math><mi id="m"></mi></math>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    @namespace m url("http://www.w3.org/1998/Math/MathML");
    m|mi { display: none; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let mi = find_by_id(&styled, "m").expect("mi");
  assert_eq!(display(mi), "none");
}

#[test]
fn namespace_rules_after_layer_statements_are_applied() {
  let html = r#"<svg><foreignObject id="svg"></foreignObject></svg>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    @layer prelude;
    @namespace svg "http://www.w3.org/2000/svg";
    svg|foreignObject { display: none; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let fo = find_by_id(&styled, "svg").expect("foreignObject");
  assert_eq!(display(fo), "none");
}

#[test]
fn import_rules_after_namespace_rules_are_ignored() {
  let css = r#"
    @namespace svg "http://www.w3.org/2000/svg";
    @import "ignored.css";
    svg|rect { display: none; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  assert!(
    !stylesheet
      .rules
      .iter()
      .any(|rule| matches!(rule, CssRule::Import(_)))
  );
}

#[test]
fn import_rules_after_layer_rules_are_ignored() {
  let css = r#"
    @import "first.css";
    @layer foo;
    @import "second.css";
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let import_count = stylesheet
    .rules
    .iter()
    .filter(|rule| matches!(rule, CssRule::Import(_)))
    .count();
  assert_eq!(import_count, 1);
}

#[test]
fn namespace_rules_after_import_and_layer_rules_are_ignored() {
  let css = r#"
    @import "first.css";
    @layer foo;
    @namespace svg "http://www.w3.org/2000/svg";
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  assert!(stylesheet.namespaces.prefixes.is_empty());
}

#[test]
fn import_rules_inside_media_rules_are_ignored() {
  let css = r#"
    @media all {
      @import "ignored.css";
      div { display: none; }
    }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let Some(CssRule::Media(media)) = stylesheet.rules.first() else {
    panic!("expected stylesheet to contain a @media rule");
  };

  assert!(!media.rules.iter().any(|rule| matches!(rule, CssRule::Import(_))));
  assert!(media.rules.iter().any(|rule| matches!(rule, CssRule::Style(_))));
}

#[test]
fn namespace_rules_after_ignored_style_rules_are_applied() {
  let html = r#"<svg><foreignObject id="svg"></foreignObject></svg>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    svg|foreignObject { display: inline; } /* invalid selector before namespace */
    @namespace svg "http://www.w3.org/2000/svg";
    foreignObject { display: block; }
    svg|foreignObject { display: none; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let fo = find_by_id(&styled, "svg").expect("foreignObject");
  assert_eq!(display(fo), "none");
}

#[test]
fn namespace_rules_after_ignored_at_rules_are_applied() {
  let html = r#"<svg><foreignObject id="svg"></foreignObject></svg>"#;
  let dom = dom::parse_html(html).unwrap();
  let css = r#"
    @unknown foo;
    @namespace svg "http://www.w3.org/2000/svg";
    foreignObject { display: block; }
    svg|foreignObject { display: none; }
  "#;
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let fo = find_by_id(&styled, "svg").expect("foreignObject");
  assert_eq!(display(fo), "none");
}
