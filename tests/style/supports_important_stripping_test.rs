use fastrender::css::parser::parse_stylesheet;
use fastrender::css::supports::supports_declaration;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;

fn find_by_id<'a>(node: &'a StyledNode, id: &str) -> Option<&'a StyledNode> {
  if node.node.get_attribute_ref("id") == Some(id) {
    return Some(node);
  }
  for child in node.children.iter() {
    if let Some(found) = find_by_id(child, id) {
      return Some(found);
    }
  }
  None
}

fn render_display(css: &str) -> String {
  let dom = dom::parse_html(r#"<div id="t"></div>"#).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let target = find_by_id(&styled, "t").expect("element with id t");
  target.styles.display.to_string()
}

#[test]
fn supports_declaration_conditions_ignore_trailing_important() {
  let without_important = r#"
    #t { display: block; }
    @supports (color: red) { #t { display: inline; } }
  "#;
  let with_important = r#"
    #t { display: block; }
    @supports (color: red !important) { #t { display: inline; } }
  "#;

  assert_eq!(render_display(with_important), render_display(without_important));
  assert_eq!(render_display(with_important), "inline");
}

#[test]
fn supports_does_not_strip_important_inside_strings() {
  let css = r#"
    #t { display: block; }
    @supports (content: "!important") { #t { display: inline; } }
  "#;
  assert_eq!(render_display(css), "inline");
}

#[test]
fn supports_does_not_strip_important_inside_nested_functions() {
  let css = r#"
    #t { display: block; }
    @supports (width: calc(1px + 2px /* !important */)) { #t { display: inline; } }
  "#;
  assert_eq!(render_display(css), "inline");
}

#[test]
fn malformed_declarations_do_not_panic() {
  // The important flag without a preceding value is invalid and should be treated as unsupported,
  // but must not panic during tokenization/stripping.
  let css = r#"
    #t { display: block; }
    @supports (color: !important) { #t { display: inline; } }
  "#;
  assert_eq!(render_display(css), "block");

  // Additional malformed values that should be handled without panicking.
  assert!(!supports_declaration("color", "!important"));
  assert!(!supports_declaration("color", "red !important !important"));
  // Unterminated function blocks are invalid CSS, but `@supports` evaluation must be resilient to
  // arbitrary/malformed authored strings.
  let _ = supports_declaration("width", "calc(1px + 2px");
  assert!(!supports_declaration("color", "\"unterminated !important"));
}
