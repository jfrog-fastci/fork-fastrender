use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::WritingMode;

fn find_tag(node: &StyledNode, tag: &str) -> Option<StyledNode> {
  if let crate::dom::DomNodeType::Element { tag_name, .. } = &node.node.node_type {
    if tag_name.eq_ignore_ascii_case(tag) {
      return Some(node.clone());
    }
  }
  for child in node.children.iter() {
    if let Some(found) = find_tag(child, tag) {
      return Some(found);
    }
  }
  None
}

#[test]
fn writing_mode_inherits_from_parent() {
  let dom = dom::parse_html(r#"<div><span>text</span></div>"#).expect("parse html");
  let css = r#"
        div { writing-mode: sideways-rl; }
        span { }
    "#;
  let stylesheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let div = find_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.writing_mode, WritingMode::SidewaysRl);

  let span = find_tag(&styled, "span").expect("span present");
  // writing-mode is inherited, so the span should adopt the parent's sideways-rl mode.
  assert_eq!(span.styles.writing_mode, WritingMode::SidewaysRl);
}

#[test]
fn legacy_ms_writing_mode_values_are_accepted() {
  let dom = dom::parse_html(r#"<div><span>text</span></div>"#).expect("parse html");
  let css = r#"
        div { -ms-writing-mode: tb-rl; }
        span { }
    "#;
  let stylesheet = parse_stylesheet(css).expect("parse css");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));

  let div = find_tag(&styled, "div").expect("div present");
  assert_eq!(div.styles.writing_mode, WritingMode::VerticalRl);

  let span = find_tag(&styled, "span").expect("span present");
  assert_eq!(span.styles.writing_mode, WritingMode::VerticalRl);
}
