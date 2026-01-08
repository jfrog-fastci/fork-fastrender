use fastrender::css::parser::parse_stylesheet;
use fastrender::dom;
use fastrender::style::cascade::{apply_styles_with_media, StyledNode};
use fastrender::style::media::MediaContext;

fn find_first<'a>(node: &'a StyledNode, tag: &str) -> Option<&'a StyledNode> {
  if let Some(name) = node.node.tag_name() {
    if name.eq_ignore_ascii_case(tag) {
      return Some(node);
    }
  }
  node.children.iter().find_map(|child| find_first(child, tag))
}

fn render_div_display(css: &str) -> String {
  let dom = dom::parse_html(r#"<div></div>"#).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let div = find_first(&styled, "div").expect("div");
  div.styles.display.to_string()
}

#[test]
fn supports_timeline_scope_dashed_ident() {
  let css = r"
    div { display: block; }
    @supports (timeline-scope: --scroller) { div { display: inline; } }
  ";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_timeline_scope_all_keyword() {
  let css = r"
    div { display: block; }
    @supports (timeline-scope: all) { div { display: inline; } }
  ";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_timeline_scope_rejects_trailing_comma() {
  let css = r"
    div { display: block; }
    @supports (timeline-scope: --scroller,) { div { display: inline; } }
  ";
  assert_eq!(render_div_display(css), "block");
}

#[test]
fn supports_timeline_scope_rejects_non_dashed_ident() {
  let css = r"
    div { display: block; }
    @supports (timeline-scope: scroller) { div { display: inline; } }
  ";
  assert_eq!(render_div_display(css), "block");
}

