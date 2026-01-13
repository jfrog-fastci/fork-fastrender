use crate::css::parser::parse_stylesheet;
use crate::dom;
use crate::style::cascade::apply_styles_with_media;
use crate::style::cascade::StyledNode;
use crate::style::media::MediaContext;
use crate::style::types::{TextBoxEdge, TextBoxTrim, TextEdgeKeyword};

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

#[test]
fn text_box_shorthand_parses_trim_and_edge() {
  let dom = dom::parse_html(r#"<div>text</div>"#).expect("parse html");
  let stylesheet =
    parse_stylesheet("div { text-box: trim-both text alphabetic; }").expect("stylesheet parses");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let node = find_first(&styled, "div").expect("div");

  assert_eq!(node.styles.text_box_trim, TextBoxTrim::TrimBoth);
  assert_eq!(
    node.styles.text_box_edge,
    TextBoxEdge::Explicit {
      over: TextEdgeKeyword::Text,
      under: TextEdgeKeyword::Alphabetic,
    }
  );
}

#[test]
fn legacy_leading_trim_and_text_edge_aliases_parse() {
  let dom = dom::parse_html(r#"<div>text</div>"#).expect("parse html");
  let stylesheet =
    parse_stylesheet("div { leading-trim: both; text-edge: cap; }").expect("stylesheet parses");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let node = find_first(&styled, "div").expect("div");

  assert_eq!(node.styles.text_box_trim, TextBoxTrim::TrimBoth);
  assert_eq!(
    node.styles.text_box_edge,
    TextBoxEdge::Explicit {
      over: TextEdgeKeyword::Cap,
      under: TextEdgeKeyword::Alphabetic,
    }
  );
}

#[test]
fn text_box_normal_resets_to_none_auto() {
  let dom = dom::parse_html(r#"<div>text</div>"#).expect("parse html");
  let stylesheet = parse_stylesheet(
    "div { text-box: trim-both text alphabetic; } div { text-box: normal; }",
  )
  .expect("stylesheet parses");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let node = find_first(&styled, "div").expect("div");

  assert_eq!(node.styles.text_box_trim, TextBoxTrim::None);
  assert_eq!(node.styles.text_box_edge, TextBoxEdge::Auto);
}

#[test]
fn text_box_shorthand_is_order_independent() {
  let dom = dom::parse_html(r#"<div>text</div>"#).expect("parse html");
  let stylesheet =
    parse_stylesheet("div { text-box: text alphabetic trim-both; }").expect("stylesheet parses");
  let styled = apply_styles_with_media(&dom, &stylesheet, &MediaContext::screen(800.0, 600.0));
  let node = find_first(&styled, "div").expect("div");

  assert_eq!(node.styles.text_box_trim, TextBoxTrim::TrimBoth);
  assert_eq!(
    node.styles.text_box_edge,
    TextBoxEdge::Explicit {
      over: TextEdgeKeyword::Text,
      under: TextEdgeKeyword::Alphabetic,
    }
  );
}

