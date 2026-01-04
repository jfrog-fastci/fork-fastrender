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
  for child in node.children.iter() {
    if let Some(found) = find_first(child, tag) {
      return Some(found);
    }
  }
  None
}

fn render_div_display(css: &str) -> String {
  let dom = dom::parse_html(r#"<div></div>"#).unwrap();
  let stylesheet = parse_stylesheet(css).unwrap();
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let div = find_first(&styled, "div").expect("div");
  div.styles.display.to_string()
}

#[test]
fn supports_animation_timeline_scroll_self() {
  let css = r"
    div { display: block; }
    @supports (animation-timeline: scroll(self)) { div { display: inline; } }
  ";
  assert_eq!(render_div_display(css), "inline");
}

#[test]
fn supports_animation_timeline_rejects_trailing_comma() {
  // Trailing commas are invalid in comma-separated lists, so this supports query must be false.
  let css = r"
    div { display: block; }
    @supports (animation-timeline: scroll(self),) { div { display: inline; } }
  ";
  assert_eq!(render_div_display(css), "block");
}

